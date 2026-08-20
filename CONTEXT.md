# Hoglet Analytics

Hoglet receives PostHog-compatible product events and turns them into durable,
queryable product analytics for an authorized project.

## Language

**Captured Event**:
A normalized product event accepted from a PostHog-compatible wire request.
_Avoid_: Raw event, ingested row

**Event Truth**:
The durable history from which every rebuildable analytical view can be
reconstructed.
_Avoid_: Source database, canonical cache

**Durable Receipt**:
Proof that every Captured Event in one accepted batch is part of Event Truth.
_Avoid_: Success response, flush receipt

**Projection**:
A rebuildable view of Event Truth, such as identity or property-catalog state.
_Avoid_: Primary data, side effect

**Checkpoint**:
The exact prefix of Event Truth incorporated into a Projection or Published
Generation.
_Avoid_: Flush time, last updated

**Published Generation**:
An immutable, internally consistent analytical snapshot visible to queries.
_Avoid_: Current files, Parquet batch

**Authorized Project**:
A project together with the authority under which a request may act on it.
_Avoid_: Token string, tenant

**Control State**:
Authoritative user-authored state such as projects, memberships, flags, saved
insights, and dashboards.
_Avoid_: Projection, metadata cache
