# Publishing Marbles events to Unibus

Marbles is a **producer** on the Unibus mesh: every tracker mutation already lands in an outbox
row inside the same transaction as its audit row, and one dispatcher delivers those rows to
exactly one sink. This document is the contract with every consumer.

## Wiring

```toml
[server.unibus]
hub_endpoint = "127.0.0.1:18790"
token_file = "/run/secrets/unibus-marbles.token"
# installation_id defaults to "fpl-cloud", producer_id to "marbles", zones to ["agents"]
```

`[server.webhook]` and `[server.unibus]` are mutually exclusive and the server refuses to start
with both: an outbox row records one delivery state, so two sinks would mark each other's work
done. The token file is read at startup, so a producer that cannot authenticate fails while
someone is watching instead of at the first merged PR.

On the hub side the same producer id must be declared with the published kinds and a route to a
consumer's capability:

```json
{ "producer_id": "marbles",
  "capabilities": ["marbles.publish"],
  "publishes": ["marbles.issue.created.v1", "marbles.issue.claimed.v1",
                "marbles.issue.status.v1", "marbles.issue.closed.v1"] }
```

with one route per kind to the consumer (e.g. `any_holder: marbles.consume`). A kind that is not
granted here is refused by the hub, so the table below and the hub config have to move together.

## Kinds

| Audit event | Kind | Fires when |
|---|---|---|
| `created` | `marbles.issue.created.v1` | an issue is created |
| `claimed` | `marbles.issue.claimed.v1` | an agent or human takes the lease |
| `status` | `marbles.issue.status.v1` | status changes, including `→ review` |
| `closed` | `marbles.issue.closed.v1` | the issue closes (with evidence or an ack) |

Everything else the audit trail records — `heartbeat`, `labels`, `updated`, `released`,
`depends_on`, `escalated` — is deliberately **not** published. It is still marked delivered in
the outbox, because the queue is ordered and an unsendable row at its head would stall the rest.

A claim gets its own kind rather than riding `status`: claiming does not change status, and a
consumer that wants "who is on this" should not have to parse a transition to find out.

## Body

One envelope shape, shared with the HMAC webhook channel (`marbles.event/1`):

```json
{ "schema": "marbles.event/1", "id": "<store_id>:<seq>", "store_id": "…",
  "company": "fpl", "project": "marbles", "issue_id": "mb-3f2a9c",
  "event": "claimed", "occurred_at": 1762392000,
  "actor": "ajmwagar", "detail": "agent", "title": "fix the retry loop" }
```

`actor` and `detail` come from the audit row that produced the event (`history.seq ==
outbound_event.seq`, written by one `log_event`), and `title` from the issue. A status event's
`detail` is the trail's own words — `→ in_review` — which is what a reader wants to see. All
three are omitted from the JSON when empty, so a consumer must treat them as optional.

`id` is store-scoped rather than globally unique: two stores may independently mint `seq = 7`.

## Delivery semantics

* At-least-once. The frame is written before the outbox row is marked delivered, so a crash can
duplicate a frame but never drop one.
* Failures back off exponentially (`1 << attempts` seconds, capped) and are recorded in
  `last_error` on the outbox row.
* One registered session is reused across events; a failed write drops it and the next delivery
  re-registers. A hub restart therefore costs one failed frame, not a permanent disconnect.
* The credential never appears in a frame body; it is read from the token file by the client and
  lives only in the registration envelope.

## Reaching the hub from a deployment

The hub binds `0.0.0.0:18790` with `allow_insecure_lan`, so `127.0.0.1` is only correct
when Marbles and the hub share a host. Marbles is deployed as a hosted service
(`marbles.fpl.dev`) through Shroud/Fab, where the hub is reachable at the Shroud bridge address
(`172.16.0.1:18790` from the `172.16.0.0/16` VM network) rather than loopback. Point
`hub_endpoint` at whatever address the container can actually reach, and confirm the path at
deploy time: the failure mode is a plain `connect hub at …` error on every delivery, visible in
`last_error` on the outbox rows.

## Verification

* `tests/unibus.rs` drives the sink against a fake hub: registration handshake, the published
  kind and body, a credential that never reaches the payload, and an unroutable endpoint for the
  skip rule.
* One live end-to-end check was run on 2026-11-09: a `marbles.issue.claimed.v1` frame published
  as producer `marbles` was consumed by Elmers and delivered to Telegram.
