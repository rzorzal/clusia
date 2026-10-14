#!/usr/bin/env python3
"""A model-free example of a Clúsia custom command.

It speaks Clúsia's JSON-lines protocol (see docs/harness-protocol.md) and answers every turn
with the prompt it was given, so it shows the whole conversation without calling a model or the
network. Set it as the program in Config > Harness to see a review run on a custom command.

  probe   {"type":"probe","protocol":1}   ->  {"type":"probe","name":"Echo",...}
  turn    {"type":"turn",...}             ->  session, tool, text, final
"""

import json
import re
import sys
import uuid

PROTOCOL = 1


def send(message):
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()


def read_message():
    line = sys.stdin.readline()
    if not line:
        return None
    try:
        return json.loads(line)
    except ValueError:
        return {}


def ask_for_permission():
    """Asks Clúsia to run `true` and says what the reviewer answered."""
    send(
        {
            "type": "permission",
            "id": "p1",
            "kind": "command",
            "detail": "true",
            "reason": "echo.py checks the permission round trip",
        }
    )
    while True:
        reply = read_message()
        if reply is None:
            return "Permission denied"
        if reply.get("type") == "permission_reply" and reply.get("id") == "p1":
            return "Permission allowed" if reply.get("decision") == "allow" else "Permission denied"


def answer_for(prompt):
    """What to say for a prompt: an empty, valid answer for a check, else an echo."""
    if "Check this pull request for security problems" in prompt:
        return "Found no security problems."
    if "Audit this pull request area by area" in prompt:
        # The prompt's own "Areas:" list is the last one: a title or a file name can say "Areas:".
        areas = re.findall(r"^- (\S+) \(", prompt.rsplit("\nAreas:\n", 1)[-1], re.MULTILINE)
        blocks = [
            "```clusia-pass\n"
            + json.dumps({"area": area, "text": "Echo looked and found nothing to report."})
            + "\n```"
            for area in areas
        ]
        return "Echo found nothing to report.\n\n" + "\n\n".join(blocks)
    if "`clusia-revalidation`" in prompt:
        # The comments come after "My comments:"; a diff line above it can look like an item.
        comments = prompt.rsplit("\nMy comments:\n", 1)[-1]
        items = re.findall(r"^- id: (\S+)$", comments, re.MULTILINE)
        blocks = [
            "```clusia-revalidation\n" + json.dumps({"item": item, "verdict": "kept"}) + "\n```"
            for item in items
        ]
        return "Echo kept every comment.\n\n" + "\n\n".join(blocks)
    return None


def turn(message):
    prompt = message.get("prompt") or ""
    send({"type": "session", "id": message.get("session_id") or str(uuid.uuid4())})
    send({"type": "tool", "summary": "Read the prompt"})
    answer = answer_for(prompt)
    if answer is None:
        answer = ask_for_permission() if "Ask permission" in prompt else "Echo: " + prompt
    send({"type": "text", "text": answer})
    send({"type": "final", "text": answer})


def main():
    message = read_message()
    if message is None:
        return
    if message.get("type") == "probe":
        send({"type": "probe", "name": "Echo", "version": "1.0", "protocol": PROTOCOL})
    elif message.get("type") == "turn":
        turn(message)


if __name__ == "__main__":
    main()
