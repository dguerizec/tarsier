# Independent observation model demand

Face landmarks, hand/gesture recognition and body pose are independently scheduled.
The daemon unions internal consumers with explicit requests owned by live event
WebSocket connections. Hiding a trace is not a global detector-off switch: another
consumer can still require that model.

## Internal consumers

| Consumer | Models |
|---|---|
| Face tracking or auto zoom | Face |
| Hands tracking | Hands |
| Enabled phone-near-mouth detector | Face + hands |
| Enabled open-palm scenario | Hands |
| Enabled face-presence scenario | Face |
| Camera identity with a background | Pose (for the existing segmentation constraint) |

The phone detector is enabled by default. Its dependencies remain active even
without an open UI or an event subscriber. Disabling its configuration explicitly
disables that gesture; a socket subscription does not override that setting.
Camera firmware gestures do not require the observation worker. Avatar face
tracking is a separate model instance and keeps its existing effect-based lifetime.

## UI consumers

The preview has separate Face, Hands and Body landmark buttons. Their selections
are stored independently; an existing all-skeleton preference initializes all
three. Visible camera previews request only their enabled traces. Hidden pages,
non-camera identities and disconnected pages release that demand. Another page
or an internal gesture may still keep the same models active.

## External consumers

Connect to the authenticated `/api/v1/events` WebSocket and send:

```json
{
  "type": "perception.subscribe",
  "models": {"face": false, "hands": true, "pose": false},
  "events": ["gesture.open_palm.held"]
}
```

Each message replaces this connection's previous request. Model flags and event
dependencies are combined. Missing flags mean false. Supported event dependencies:

- `gesture.open_palm.held`: hands.
- `gesture.phone_near_mouth.started` / `.ended`: face and hands.
- `face.present.started` / `.ended`: face.

The server acknowledges with `type: "perception.subscription"` and the resolved
models for that connection. Unknown events, fields or invalid flags are rejected
with `perception.subscription.error`, preserving the previous request. At most
16 event names and 4096 bytes are accepted per demand message.

Send empty `models` and `events` to release demand while keeping the socket, or
close it. Requests are scoped to the socket and removed on disconnect/cancellation;
a reconnect must declare its demand again. This controls computation, not delivery
filtering: the existing state/event broadcast protocol is unchanged. Merely opening
a socket or reading state does not implicitly activate every model. External
clients that need continuous observations must explicitly request them.

`GET /api/v1/perception/demand` returns the aggregate model flags and is available
to authenticated API readers and the scoped worker credential. It does not itself
register demand. `state.perception.active_models` and telemetry's
`pipeline_context.perception.active_models` report the models used by the latest
worker observation. Check worker connectivity/freshness as well as those flags.
Inactive is distinct from active with no detection.

## Worker behavior and boundaries

The observation thread polls demand at most four times per second. Each model is
initialized on first use, skips inference as soon as its demand disappears, and
is closed after two seconds without demand. Rapid reactivation reuses a loaded
model. Delegate selections remain independent and unchanged. Failure to obtain a
valid demand response conservatively enables all three models until a subsequent
successful poll, preserving detection when communicating with an older daemon.

Inactive models publish empty landmarks and negative detections. Inactive hands
also clear the current gesture, last-hand timestamp and remembered peak gesture.
The server discards positive detections/landmarks supplied for inactive models.
Pose constraints are replaced with an empty mask when pose is inactive, preserving
fail-closed camera background behavior until fresh pose results arrive.

The observation heartbeat, frame preparation/shared copy, depth/avatar queue
submissions and preview encoding still run. This change does not suspend the
entire worker or implement consumer-driven video transport.
