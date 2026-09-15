# Preparatory management snapshot boundary

This crate implements only authenticated `GET /management/v1/snapshot`.
The response retains `scope: "durable_store_foundation"`: it is historical
durable state, not complete A3 status, fresh ownership proof or readiness.
Every other path is absent; other methods are denied. There are no engine,
coordinator, inference, mutation, event, CLI or listener integration callbacks.

The trusted service must supply independently generated management and inference
credentials through `ManagementCredentials::from_trusted_resolver`. Both must
be 32–256 ASCII bearer-token characters; equal credentials are refused. The
constructor validates syntax and separation, NOT entropy or provenance. Never
resolve credentials from public deployment configuration or HTTP inputs. No
credential file loader is supplied yet. The credential object retains only a
SHA-256 management-token digest, compared with `subtle` constant-time equality;
it implements neither Debug nor Serialize. Missing, duplicate and malformed
Authorization fields and inference credentials fail before provider access.

`snapshot_router` must be mounted exclusively on a separate management listener.
Production composition must enforce loopback or configured TLS, distinct ports,
protected credential resolution, transport limits and shutdown. This crate does
not bind a listener or provide an insecure remote-listener override. No CORS or
cookie authentication is enabled.

`StoreSnapshotSource` owns an already-open Store; requests do not open databases,
run migrations or begin coordinator sessions. Reads use Store's bounded coherent
read transaction. Each router admits at most two blocking reads, including
cancelled requests whose workers are still running; excess requests get versioned
429 `queue_full`. Instantiate one router for the service, not one per request.
Provider errors are mapped to fixed versioned errors without source diagnostics.
No raw credentials or errors are serialized. Responses disable caching.

Remaining A3 work: protected service credential loader/listener composition,
complete snapshot projection, bounded authenticated replay/live SSE (including
cursor expiry, heartbeat, reconnection and slow-client handling), mutation and
operation routes, and CLI cutover. `/management/v1/events` deliberately returns
404 until that protocol exists; no alternate JSON protocol is invented.
