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
in memory, but that is an instruction, not a guarantee. Two things narrow the
gap. Anything off the network arrives wrapped in a marked boundary saying it
is somebody else's writing, to be read and never obeyed, which is a warning
sitting next to the payload rather than two thousand tokens before it. And
any tool result carrying the phrases an injection needs, in a file as much as
on a page, gets a line pointing at the attempt, because refusing quietly
still leaves the user not knowing someone tried.

None of that is a guarantee either. The model is the thing being persuaded,
and the last defence is the same as the first: every action that touches your
machine is shown to you in full and, unless you turned that off, asks first.
