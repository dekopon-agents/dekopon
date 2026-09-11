"""OS-process proof of rendered init permissions, not a substitute for ipc_process.rs.

All inputs are disposable fixtures. Both private mounts are deliberately exposed to
both test UIDs here: permissions must deny access even beyond the chart's mount isolation.
"""
import errno
import os
import select
import signal
import socket
import stat
import struct
import time


def denied(operation, label):
    try:
        operation()
    except OSError as error:
        assert error.errno in (errno.EACCES, errno.EPERM), (label, error)
        print("PASS denied " + label, flush=True)
    else:
        raise AssertionError("unexpected access: " + label)


def read(path):
    with open(path, "rb") as handle:
        handle.read(1)


def write(path):
    with open(path, "wb"):
        pass


def owned_file(path, mask):
    parent = os.path.dirname(path)
    st = os.lstat(parent)
    assert stat.S_ISDIR(st.st_mode) and st.st_uid == os.geteuid()
    assert stat.S_IMODE(st.st_mode) == 0o700
    while True:
        st = os.lstat(parent)
        assert stat.S_ISDIR(st.st_mode)
        assert st.st_mode & 0o022 == 0 or st.st_mode & 0o1000 != 0
        if parent == "/":
            break
        parent = os.path.dirname(parent)
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
    try:
        st = os.fstat(fd)
        assert stat.S_ISREG(st.st_mode) and st.st_nlink == 1
        assert st.st_uid == os.geteuid() and st.st_mode & mask == 0
    finally:
        os.close(fd)


path = "/run/dekopon/broker.sock"
ready_read, ready_write = os.pipe()
children = []


def spawn(uid, action):
    pid = os.fork()
    if pid == 0:
        try:
            os.setgroups([65534])
            os.setgid(uid)
            os.setuid(uid)
            action()
        except BaseException:
            import traceback
            traceback.print_exc()
            os._exit(1)
        os._exit(0)
    children.append(pid)
    return pid


def broker():
    for file in ("broker.yaml", "policies.cedar", "broker-credentials.yaml"):
        owned_file("/etc/dekopon/" + file, 0o077)
    owned_file("/etc/dekopon-storage-key/storage-key.yaml", 0o077)
    write("/var/lib/dekopon-provider-storage/private")
    for file in ("/etc/dekopon-gateway/dekopond.yaml", "/gateway-state/chatgpt-auth.json"):
        denied(lambda: read(file), "broker reading " + file)
        denied(lambda: write(file), "broker writing " + file)
        denied(lambda: os.unlink(file), "broker replacing " + file)
    with socket.socket(socket.AF_UNIX) as server:
        server.settimeout(10)
        server.bind(path)
        os.chown(path, -1, os.stat("/run/dekopon").st_gid)
        os.chmod(path, 0o660)
        server.listen(1)
        os.write(ready_write, b"1")
        connection, _ = server.accept()
        with connection:
            connection.settimeout(10)
            _, uid, _ = struct.unpack("3i", connection.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12))
            assert uid == 65533, uid
            assert connection.recv(1) == b"g"
            connection.sendall(b"b")
    os.unlink(path)


def gateway():
    owned_file("/etc/dekopon-gateway/dekopond.yaml", 0o022)
    owned_file("/gateway-state/chatgpt-auth.json", 0o077)
    for file in ("/etc/dekopon/broker.yaml", "/etc/dekopon/policies.cedar",
                 "/etc/dekopon/broker-credentials.yaml", "/etc/dekopon-storage-key/storage-key.yaml",
                 "/var/lib/dekopon-provider-storage/private"):
        denied(lambda: read(file), "gateway reading " + file)
        denied(lambda: write(file), "gateway writing " + file)
        denied(lambda: os.unlink(file), "gateway replacing " + file)
    denied(lambda: os.unlink(path), "gateway unlinking broker socket")
    denied(lambda: os.rename(path, path + ".old"), "gateway renaming broker socket")
    denied(lambda: write("/run/dekopon/replacement"), "gateway creating IPC sibling")
    denied(lambda: os.chmod(path, 0o600), "gateway changing socket permissions")
    st = os.stat(path)
    assert stat.S_ISSOCK(st.st_mode) and st.st_nlink == 1
    assert st.st_uid == 65532 and st.st_gid == 65534 and stat.S_IMODE(st.st_mode) == 0o660
    with socket.socket(socket.AF_UNIX) as client:
        client.settimeout(10)
        client.connect(path)
        _, uid, _ = struct.unpack("3i", client.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12))
        assert uid == 65532, uid
        client.sendall(b"g")
        assert client.recv(1) == b"b"
    print("PASS real OS processes UID 65533 -> UID 65532 via group 65534 (layout fixture)", flush=True)


try:
    spawn(65532, broker)
    assert select.select([ready_read], [], [], 10)[0], "broker readiness timeout"
    assert os.read(ready_read, 1) == b"1"
    spawn(65533, gateway)
    deadline = time.monotonic() + 15
    while children and time.monotonic() < deadline:
        for pid in children[:]:
            done, status = os.waitpid(pid, os.WNOHANG)
            if done:
                children.remove(pid)
                assert os.waitstatus_to_exitcode(status) == 0, (pid, status)
        time.sleep(0.05)
    assert not children, "child deadline exceeded"
finally:
    for pid in children:
        os.kill(pid, signal.SIGKILL)
        os.waitpid(pid, 0)
    os.close(ready_read)
    os.close(ready_write)
