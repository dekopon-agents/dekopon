import json
import re


def read_messages(request, key, contains_credential):
    if (request.path != "/v1/chat/completions"
            or request.headers.get_all("Authorization") != [f"Bearer {key}"]):
        raise ValueError("Unexpected model endpoint or fake credential")
    size = int(request.headers.get("Content-Length", "0"))
    if not 0 < size <= 1024 * 1024:
        raise ValueError("Model fixture request exceeds its body limit")
    data = json.loads(request.rfile.read(size))
    headers = [(name, value) for name, value in request.headers.items()
               if name.lower() != "authorization"]
    if contains_credential(json.dumps(headers) + json.dumps(data)):
        raise ValueError("A credential reached model input or the wrong HTTP client")
    return data["messages"]


def latest_user_lines(messages):
    index = next(i for i in reversed(range(len(messages))) if messages[i]["role"] == "user")
    lines = messages[index]["content"].splitlines()
    return index, [re.sub(r"<@[A-Za-z0-9]+>\s*", "", line).strip() for line in lines]


def bash_call(command):
    return {"role": "assistant", "content": None, "tool_calls": [{
        "id": "call-1", "type": "function", "function": {
            "name": "bash", "arguments": json.dumps({"command": command}),
        },
    }]}


def completion(message, prompt_tokens=100, completion_tokens=20):
    return {"choices": [{"message": message}], "usage": {
        "prompt_tokens": prompt_tokens, "completion_tokens": completion_tokens,
        "total_tokens": prompt_tokens + completion_tokens,
    }}
