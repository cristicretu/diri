# HTTP APIs in Diri

When the client lists `act`, pass the action names below to Diri’s `act` tool with an `arguments` object. Use `get_skill` with `action` for its schema, or with no arguments to list actions and guides. In a Diri terminal, `"$DIRIJOR_CLI" mcp-tools` prints the CLI catalog.

Diri's right panel has an **API** tab: a request builder (method, URL, params, headers, body, auth), a response viewer (status, time, size, headers, folding JSON), saved collections, history and environments with `{{variables}}`. Each project keeps its own.

## Show the person the endpoint

When you build, run or debug an HTTP API, open the request in **your** session's API tab with `open_api_request` instead of only printing a curl command:

- After starting a dev server, open the endpoint you added or changed, e.g. `{"url": "http://localhost:3000/api/health", "auto_send": true}`.
- After changing a handler, open the request that exercises it, prefilled: `{"method": "POST", "url": "{{baseUrl}}/items", "json": {"name": "tea"}, "variables": {"baseUrl": "http://localhost:3000"}}`.
- Reproducing a bug report: open the failing request with its headers and body so the person can send it and see the response.

## Rules

- `auto_send: true` is honoured for `GET` only. `POST`, `PUT`, `PATCH`, `DELETE` and the rest always wait for the person to press Send: never ask for them to be sent on your behalf through other means to get around this.
- Put tokens in `variables` and list them in `secrets`, then refer to them as `{{token}}` (for example `"headers": {"Authorization": "Bearer {{token}}"}`). Secrets are masked in the panel. Never paste a real production credential you were not given for this purpose.
- `variables` merge into the project's environment named by `environment` (default: the active one, or `Local`), so later requests can reuse them.
- The tool opens the tab; it does not return the response. To check an endpoint yourself, use your own tools (for example `curl`) and use the API tab for the person.
- Prefer `localhost` URLs for local servers; a URL without a scheme gets `http://` for local hosts and `https://` otherwise.
