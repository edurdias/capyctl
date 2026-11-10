# CapyCTL 0.1.10

CapyCTL 0.1.10 fixes one problem found in 0.1.9: the router reordered the
keys of every request it forwarded, which changed the answers of decision
models on SGLang.

## Fixes

- **Requests reach the engine as you sent them.** The router used to forward
  every request with its JSON object keys sorted, which reordered the
  questions of a `/v1/systemone` decision (SGLang numbers them in its prompt
  in the order you send them) and changed answers, and reordered the
  properties of a chat request's JSON schema. The engine now receives your
  request's bytes, with only the served model name and the fields CapyCTL
  sets changed. A request that repeats a key in the same object is refused
  with `invalid_request` (issue #220).
