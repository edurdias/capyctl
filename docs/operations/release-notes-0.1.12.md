# CapyCTL 0.1.12

CapyCTL 0.1.12 fixes one problem found on 0.1.11: a model on vLLM 0.33.0
would not start. The launcher runs on the machine with the GPU, so upgrade
each host or standalone machine that runs vLLM 0.33.0. It changes no
protocol: a 0.1.12 server and 0.1.11 hosts work together, and the other way
round.

## Fixes

- **CapyCTL launches vLLM 0.33.0.** 0.1.11 accepted a vLLM 0.33.0
  installation at `capyctl engine add`, but every model launch on it failed
  at startup, because vLLM 0.33.0 moved a function CapyCTL's launcher calls.
  The launcher now finds it in either place; earlier vLLM versions launch as
  before. vLLM 0.33.0 is still shown as a custom build (`CUSTOM yes`) until
  it is qualified.
