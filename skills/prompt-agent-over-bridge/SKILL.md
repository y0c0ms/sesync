---
name: prompt-agent-over-bridge
description: "Use when the user asks you to ask, tell, hand work to, or check on an agent running on another of their machines over the sesync bridge, or to read what that agent answered."
---

# Prompt an agent on another machine over the sesync bridge

The user's machines run sesync bridges, and the phone app's "can see" switch lets one machine see another's
conversations. From a machine that sees another, `sesync machine` lists the conversations running there, sends a
prompt into one of them as a prompt turn (the same call the phone's composer makes), and reads its answer. Use it
when the user says "ask the agent on my laptop to…", "tell the desktop's session to…", "what did the other agent
say?".

## Commands

```
sesync machine list [<machine>]                                   # conversations there: state, title, harness, folder, id
sesync machine read <machine> <conversation> [<count>]            # its last messages (default 10), oldest first
sesync machine prompt <machine> <conversation> "<text>" [--open] [--wait[=<seconds>]]
sesync machine prompt <machine> <conversation> - [--open] [--wait]   # the text from stdin (long prompts)
```

- `<machine>` is a name from `sesync machine list`. `<conversation>` is the id from the list (safest), its exact
  title, or a unique part of the title.
- The state column: `working` (busy: do not prompt it; wait or ask the user), `waiting` or `done` (ready for a
  prompt), `blocked` (it waits on a question in its pane: a person answers it there), `closed` (not running there).
- `--wait` returns once that agent has answered and stopped working, and prints its answer (default limit 600 s,
  `--wait=1800` for long work). Without it the command returns as soon as the prompt is in.
- `--open` resumes a closed conversation on that machine first, under its own session (no fork, no second agent).
  Use it only when the user asked to resume that conversation, or it is clearly the one meant.

## How to do it

1. `sesync machine list <machine>`; pick the conversation the user named. Two equal titles: use the id. If
   none fits, show the list and ask.
2. If it is `working`, do not interrupt it: tell the user, or wait and list again.
3. Write the prompt so the other agent can act on it alone: it does not see your conversation. Say who is asking
   ("From the agent on <this machine>, for <user>:"), what you need, and what to send back. Never paste secrets,
   tokens or keys into a prompt.
4. Send it with `--wait` when you need the answer to go on; otherwise send it and tell the user it is queued.
5. Report the answer, or that it timed out (then `sesync machine read` later), or that it is blocked (the user
   answers in that pane: `sesync machine attach <machine> <conversation>`).

## Direction and limits

- A machine can only prompt a machine it can reach: `sesync machine list` shows a machine this one cannot dial as
  `unreachable`. Then only the other direction works. Do not open ports, change routes, firewalls or a VPN to work
  around it; tell the user.
- The other machine must run a sesync with these commands' bridge support (`sessionId` in its listing); an older
  one still takes prompts to open conversations, and `read` asks the user to update it.
- A prompt is a full turn for that agent, with its tools and its approvals on that machine. Send only what the
  user asked for.
- Reference: `docs/BRIDGE.md`, section "Machines", in the sesync repository.
