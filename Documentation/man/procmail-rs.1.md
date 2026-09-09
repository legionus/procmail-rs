<!-- SPDX-License-Identifier: MIT -->
<!-- Copyright (C) 2026  Alexey Gladkov <legion@kernel.org> -->

# NAME

procmail-rs - bounded procmail-compatible mail filtering

# SYNOPSIS

**procmail-rs** **check** **--config** *PATH* [**--set** *NAME=VALUE*]...

**procmail-rs** **explain** **--config** *PATH* [**--set** *NAME=VALUE*]...

**procmail-rs** **filter** **--config** *PATH* [**--set** *NAME=VALUE*]...

**procmail-rs** **--help**

**procmail-rs** **--version**

# DESCRIPTION

**procmail-rs** validates and evaluates a deliberately limited procmail rc
language. In **filter** mode it reads exactly one mail message from standard
input and delivers it only to destinations selected explicitly by that rc
file.

The message is handled as arbitrary bytes. Configured limits are enforced
while input is read; an unterminated header line or body cannot grow memory or
staging storage beyond those limits. Maildir publication and mbox append are
completed before delivery is reported as successful.

This program is not a privileged local-delivery agent. It does not select a
system mailbox, use `/var/spool/mail/$LOGNAME`, forward through sendmail, or
provide an implicit fallback destination.

Supported targets are 32-bit and 64-bit Linux and 64-bit FreeBSD. Linux keeps
pending Maildir data unnamed until publication. FreeBSD uses an exclusively
created named file in `tmp`, restricts directory ownership and permissions,
and checks the open file identity around publication; it cannot completely
exclude hostile pathname replacement between a check and an operation.

# COMMANDS

**check**

: Validate the root rc file and every runtime rc file whose path can be
  determined without a message. Standard input is not read. A message-derived
  `INCLUDERC` or `SWITCHRC` path produces a bounded warning and is checked only
  if **filter** later reaches it.

**explain**

: Perform the same validation as **check**, then write a value-free execution
  plan to standard output. Commands are described by type but are not run and
  their text is not printed.

**filter**

: Read one message from standard input, evaluate recipes in order, and perform
  the selected explicit deliveries. An original message delivered only by
  copy recipes remains undelivered.

# OPTIONS

**--config** *PATH*

: Select the root rc file. The file must be UTF-8 and no larger than 1 MiB.
  It is parsed before **filter** reads standard input.

**--set** *NAME=VALUE*

: Supply one policy-checked initial rc value. The option may occur at most 256
  times. Rc assignments can replace supplied values in statement order.
  Ambient process variables are not imported.

**-h**, **--help**

: Print brief command-line help.

**-V**, **--version**

: Print the package version.

# INITIAL VALUES

`HOME` and `LOGNAME` are obtained from the passwd entry for the current uid.
`HOST` is obtained from the current system node name. `PROCMAIL_VERSION`
contains the **procmail-rs** package version. Ambient variables with those
names cannot override the initial values.

Other external values must be admitted explicitly:

```
procmail-rs check --config rules.rc --set ACCOUNT=work
```

# FILTERING EXAMPLE

Create the destination Maildir before filtering; **procmail-rs** does not
create or repair its `tmp`, `new`, and `cur` directories.

```
MAILDIR=/home/user/Mail

:0
* ^List-Id:.*<project\.example>
lists/project/

:0
inbox/
```

Validate the configuration, inspect its plan, and then filter a message:

```
procmail-rs check --config rules.rc
procmail-rs explain --config rules.rc
procmail-rs filter --config rules.rc <message.eml
```

# EXIT STATUS

**0** (`EX_OK`)

: Validation, explanation, or complete filtering succeeded.

**65** (`EX_DATAERR`)

: Message input was rejected, for example because a message limit was
  exceeded. Retrying the same bytes unchanged will not help.

**70** (`EX_SOFTWARE`)

: An internal failure prevented a reliable result.

**73** (`EX_CANTCREAT`)

: A permanent destination problem prevented delivery.

**75** (`EX_TEMPFAIL`)

: A temporary resource, locking, process, or delivery failure occurred.

**78** (`EX_CONFIG`)

: The command line or rc configuration is invalid.

**79** (`PROCMAIL_RS_UNDELIVERED`)

: No final recipe delivered the original message. Copy destinations may
  already have been published.

**129**, **130**, **131**, or **143**

: Filtering was interrupted by `SIGHUP`, `SIGINT`, `SIGQUIT`, or `SIGTERM`,
  respectively. An active external command process group is terminated and
  `TRAP` is not run.

An invoking MTA must retain the message unless **filter** exits with status 0.
A retry after one of several copy destinations was published can create a
duplicate at that destination. A signal observed before publication prevents
it; a signal arriving after an atomic Maildir publication or completed mbox
append cannot retract that delivery and a retry may duplicate it.

# FILES

There is no automatically discovered rc file. The root file is always named
with **--config**. Runtime files may be selected explicitly by `INCLUDERC` and
`SWITCHRC` statements.

# SECURITY NOTES

Mail, configuration text, command-line values, paths, filesystem state, and
child output are treated as potentially hostile and are bounded by the
program. Commands written in a trusted rc file are trusted code: they are not
sandboxed. Use namespaces, cgroups, or a service manager when those commands
need additional containment.

Trace output is metadata-only by default. Rc values are logged only after an
explicit `LOGDETAIL=values`; message bodies and header values are not enabled
by that setting.

# SEE ALSO

**procmail-rs.rc**(5), **maildir**(5), **mbox**(5)

The source distribution also contains `Documentation/Compatibility.md`,
`Documentation/Delivery.md`, and `Documentation/Limits.md`.
