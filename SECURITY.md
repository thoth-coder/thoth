# Security Policy

## Reporting

Report security issues privately through GitHub Security Advisories (the
"Report a vulnerability" button on the Security tab). Don't open a public
issue for anything exploitable.

## Threat model

thoth runs a language model with tool access on your own machine, against
your own files. Model output is untrusted input. What thoth does about it:

- File edits and overwrites are rejected unless the model actually read the
  file. This is enforced in code, not in the prompt.
- Write, edit and shell actions need interactive approval. "Always allow"
  only lasts for the session and only covers that one tool.
- Every shell command line and every file diff is displayed, including
  auto-approved ones.
- Fetched web content is never executed. Memory (`remember`) is plain text
  you can audit with `/memory`.

If you always-allow `shell`, the model can run arbitrary commands as your
user. Watch the displayed commands, and prefer per-call approval when
working on anything you don't trust.

## Scope

thoth only talks to the model server you configured (localhost by default)
and, for web_search/web_fetch, to the sites involved. There is no telemetry.

Prompt injection through fetched web pages is a known risk for every agentic
tool. thoth tells the model to treat web content as data and never store it
in memory, but that is an instruction, not a guarantee.
