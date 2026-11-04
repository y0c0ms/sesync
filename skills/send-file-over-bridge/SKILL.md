---
name: send-file-over-bridge
description: "Use when the user asks you to send, give, share or hand over a file, folder or zip from this machine to the machine they are prompting from (work PC or phone) over the sesync bridge."
---

# Send a file or zip to the user over the sesync bridge

The user is prompting you from another machine (the work PC, or the phone) through the sesync bridge on this
machine. They cannot open files on this machine, so "send me X" means: make X reachable by a path, then give
them the one command that pulls it. Do not paste a file's contents into chat when they asked for the file.

## How it moves: the user pulls it

The bridge serves **any file or folder on this host by absolute path** (`files.startDownload`). A folder comes
as a zip made on the fly, symlinks left out. You do not need to zip a single file or folder, and you do not need
to copy it anywhere. You need the exact absolute path.

This is the only direction that works from here. The work PC reaches this machine through a tunnel
(`ws://127.0.0.1:16768` there); this machine does **not** hold a working link to the work PC's bridge (its
`10.10.20.4:6768` is closed from here), so `sesync machine send` from this machine toward the work PC fails.
Do not try it, and do not open ports, add routes or change the VPN to make it work.

1. Resolve the path and check it exists and its size (`ls -l`; `du -sh` for a folder). Over 2 GiB is refused:
   say so, and offer to split it or send a subset.
2. If the user named several files, make one path: copy them into one folder, or
   `mkdir -p ~/sesync-outbox && zip -r ~/sesync-outbox/<name>.zip <files>`. Never include secrets, `.env`
   files, `.git`, tokens or credentials unless the user asked for exactly those. When unsure what a folder holds,
   list it and ask first.
3. Reply with the command, filled in, for the user to run **on the machine they are sitting at**:

   ```
   sesync machine get yocomsMain /absolute/path/on/this/machine [<folder there>]
   ```

   `yocomsMain` is this machine's name in the work PC's machine list (`sesync machine list` there shows it).
   With no folder the file lands in the directory they run it from. A folder arrives as `<name>.zip`. Nothing is
   overwritten: a taken name becomes `name (1).ext`.
4. State the path, the size, and that a folder is zipped on the way. Do not say it was delivered: only the
   user's `get` moves the bytes. If they report an error: `not_found` means the path is wrong or not absolute;
   no answer means the bridge here is down (`systemctl --user is-active sesync-bridge`) or the work PC's link is
   (`sesync machine list yocomsMain` there).

From the phone, the same path is served by `files.startDownload`; the app's download action takes the same
absolute path.

## Files the user sends to this machine

Files sent with `sesync machine send yocomsMain <file>` (or from the phone) land in this machine's **inbox
folder**: the `inbox` value in `~/.config/sesync/transfer.json`, default `~/sesync-inbox`. The user changes it in
the sesync TUI, last tab, Settings. When the user says "I sent you a file", look there first, newest first
(`ls -lt`), and do not assume the name: a taken name gets a number.

## Limits and safety

- A paired device can read any path on this host. Send only what was asked for, and say what you are exposing
  when it is not obvious (a whole home directory, a `.config`, a repository with a `.env`).
- 2 GiB per file or folder, 8 transfers at once, a transfer idle for 5 minutes is dropped, symlinks inside a
  folder are left out of its zip.
- Reference: `docs/BRIDGE.md`, section "File transfer", in the sesync repository.
