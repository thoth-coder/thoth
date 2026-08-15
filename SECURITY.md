# Security Policy

## Reporting

Report security issues privately through GitHub Security Advisories (the
"Report a vulnerability" button on the Security tab). Don't open a public
issue for anything exploitable.

## Threat model

thoth runs a language model with tool access on your own machine, against
your own files. Model output is untrusted input. What thoth does about it:

- File edits and overwrites are rejected unless the model actually read the
  file, and deleting or renaming one carries the same condition, so neither
  is a way around it. This is enforced in code, not in the prompt.
- Write, edit, delete, move and shell actions need interactive approval.
  "Always allow" is saved per project in
  `~/.thoth/projects/<key>/allow.json` and survives restarts, so it is worth
  reviewing with `/allow` (`/allow reset` clears it). It is scoped to what
  you saw: one program for shell (`shell:cargo`), one host for `web_fetch`,
  and for a file outside the working directory the one directory it sits in.
- Every shell command line and every file diff is displayed, including
  auto-approved ones.
- Every file a request changes is snapshotted first, so `/undo` puts the
  whole request back. A file edited by someone else since is reported and
  left alone rather than overwritten.
- Fetched web content is never executed. Memory (`remember`) is plain text
  you can audit with `/memory`.

If you always-allow `shell` for a program, the model can run that program
with any arguments as your user, in this project, until you clear it. Watch the displayed commands, and prefer per-call approval when
working on anything you don't trust.

## Scope

thoth only talks to the model server you configured (localhost by default)
and, for web_search/web_fetch, to the sites involved. There is no telemetry.

Prompt injection through fetched web pages is a known risk for every agentic
tool. thoth tells the model to treat web content as data and never store it
in memory, but that is an instruction, not a guarantee.
