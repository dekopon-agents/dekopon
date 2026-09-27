# dekopon-provider-random

Guest binding for `dekopon:random/source@0.1.0`. A provider imports this interface explicitly and calls `get_random_bytes(length)` during an authorized invocation. The broker obtains bytes from the OS CSPRNG and refuses requests above 4096 bytes; a zero-length request returns an empty vector. The binding has no guest-side seed or fallback. Describe and command resolution must remain pure. Consumers requesting larger buffers should issue multiple bounded calls.
