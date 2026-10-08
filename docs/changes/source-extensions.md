# Status: Plain HTTP by host approval, download URLs by secret reference — 2026-10-08 (branch `feat/source-extensions`)

Owner approval 2026-10-08, recorded as the ADR 0008 amendment of that date. An `http` model
source may now name a plain `http://` URL; the host fetches it only when it approves plain HTTP
(`model_sources.plain_http: allowed`, `--model-sources-plain-http`,
`CAPYCTL_MODEL_SOURCES_PLAIN_HTTP`; flag over environment over YAML; default `denied`, encoded
only when allowed, so existing host policies and revisions are byte-identical), still inside
`http: allowed` and `allowed_hosts`, still verified against its `sha256`. A fetch that starts on
HTTPS is never redirected down to plain HTTP. An `http` source may instead state
`url_ref: secret://<name>` (a deployment-document field, YAML only like every deployment
field): the host reads the URL from `<state_dir>/secrets/<name>` (owner-only) at the moment of
the fetch and checks it against its own policy first (`denied` otherwise,
`secret_unavailable` for a missing, loose or non-URL secret). The document, revision, ledger,
status and logs hold the reference only; the store directory stays `sources/http/<sha256>`.
The sources store's log lines pass through the new shared
`capyctl_domain::redact::redact_urls` (userinfo, query and fragment become `<redacted>`).

Tests (T14 T37): `plain_http_sources_need_the_hosts_approval`,
`a_secret_url_reference_is_all_a_declaration_holds` (config, runtime failures before the
change), `plain_http_follows_flag_env_document_default`,
`plain_http_and_secret_url_sources_resolve_into_the_store`,
`a_secret_url_is_fetched_verified_and_never_persisted_or_logged` (no stored file, status,
failure or log line holds the URL), `plain_http_sources_are_fetched_only_where_the_host_approves`
(agent, loopback origin), and the `redact` unit tests (domain). CPU tests only; they are not
qualification. A live fetch from a plain-HTTP mirror and from a presigned URL on a lab host is
pending.

# Release note: Model downloads

- **Plain `http://` downloads where the host approves them.** An `http`
  model source may name an `http://` URL, for example an internal mirror.
  A host fetches it only when it allows plain HTTP:
  `model_sources.plain_http: allowed`, `--model-sources-plain-http allowed`
  on `start host` and `start standalone`, or
  `CAPYCTL_MODEL_SOURCES_PLAIN_HTTP=allowed`. The default stays HTTPS only,
  and the download is still checked against its `sha256`. See
  [configuration](../operations/configuration.md).
- **A download URL kept as a secret.** An `http` source may state
  `url_ref: secret://<name>` instead of `url`, naming an owner-only file
  `<state dir>/secrets/<name>` on the host that holds the URL, for example a
  short-lived presigned link. The deployment, its revisions, status and logs
  only ever hold the reference; the host reads the URL when it downloads
  and checks it against its own `model_sources` rules first. Replacing an
  expired link means rewriting the file; the deployment does not change.
  Like every deployment field, it is set in the deployment YAML only.
