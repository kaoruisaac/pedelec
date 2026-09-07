# Shape Rain

Shape Rain is a Vite + SolidJS demo that sends natural-language shape requests to Pedelec. The agent calls validated frontend tools to drop PixiJS-rendered, Matter.js-powered glass-like basic shapes and closed/custom polygons.

## Setup

```bash
npm install
npm run dev
```

The dev server defaults to `http://127.0.0.1:5174`.

Development uses the repository-local SDK source; the production build resolves the `@kaoruisaac/pedelec` npm dependency declared as `latest`.

Shape Rain uses the SDK default page-scoped session lifecycle. Refreshing or closing the page disconnects the SDK connection, and Pedelec automatically ends the old desktop thread.

## Pedelec prerequisites

1. Install and enable the Pedelec Chrome Extension.
2. Start the Pedelec Desktop App.
3. Confirm Chrome Native Messaging is registered from the Desktop App.
4. Configure a default provider and its effort profiles in Desktop App Settings, or select an available provider and effort level in Shape Rain.
5. Open the dev URL in Chrome and approve the origin in the extension popup when prompted.

## Test flow

Type a request such as `drop five blue circles and two pink triangles`, then press Enter. The input is sent to Pedelec, and shapes are created only when the agent returns a valid `spawn_basic_shapes` or `spawn_closed_polygons` frontend tool call. Shape Rain displays assistant responses incrementally when the provider emits chat deltas and reconciles them with the completed message.

The diamond toolbar button drops a local demo batch for rendering/physics checks. It is not the formal natural-language flow.

## Limits

- Supported basic shapes: circle, square, rectangle, triangle, pentagon, hexagon, star, and capsule; closed/custom polygons are also supported.
- The demo supports both 2D and 3D rendering modes; it does not provide freeform drawing, dragging, or an object editor.
- Commands are clamped to 12 shapes per item and 36 shapes per command.
- The world keeps at most 160 active physics objects and removes old/offscreen objects.
