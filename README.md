<p align="center">
  <img src="docs/logo.svg" width="200" alt="tau logo">
</p>


*Note: Not ready yet for public consumption. For informational purposes only.*

# Tau coding agent

> Tau is like [Pi][pi], but twice as much.

[Pi agent][pi] is truly a breath of fresh air in the AI coding space,
but it doesn't go far enough. Tau is twice as Unix-like,
which makes it just right.

Instead of being built on top of a Typescript (JS) runtime, Tau builds on top
the most venerable, powerful and ubiquitous runtime there is: Unix itself.

Tau runs all its components as standalone Posix processes, communicating
over stdio/RPC.

Components include:

* UI
* harness
* LLM API
* extensions

This architecture has important benefits:

* Starting a component is just running a process with all the power it brings.
* Components can be system-provided, which pairs well with technologies like NixOS.
* Each component can be easily ran on a different VM or system.
  Remote execution is as simple as prefixing a component command with `ssh user@host -c`.
* Each component can be sandboxed individually using tools like bubblewrap, docker, jails, landlock, etc.
  according to its actual needs.
* Components can be implemented in any programming language.
* In theory with a little shim Tau could run [Pi][pi] extensions.
* Avoids bring in web tech where it doesn't belong.

[pi]: https://shittycodingagent.ai/


## Contact

* [I don't want your PRs anymore](https://dpc.pw/posts/i-dont-want-your-prs-anymore/)
* [`#support:dpc.pw` Matrix channel](https://matrix.to/#/#support:dpc.pw)

## Status

[![asciicast](https://asciinema.org/a/KCpgTfSOQGEEuWJ1.svg)](https://asciinema.org/a/KCpgTfSOQGEEuWJ1)

## AI usage disclosure

[I use LLMs when working on my projects.](https://dpc.pw/posts/personal-ai-usage-disclosure/),
though due to its nature this project is more "vibed" than usual,
especially in its infancy.
