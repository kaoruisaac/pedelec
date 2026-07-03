# Shape Rain

Shape Rain is a Vite + SolidJS demo that sends natural-language shape requests to Pedelec. The agent should call the frontend tool `spawn_basic_shapes`; the app validates the command and drops PixiJS-rendered, Matter.js-powered glass-like basic shapes.

## Setup

```bash
npm install
npm run dev
```

The dev server defaults to `http://127.0.0.1:5174`.

## Pedelec prerequisites

1. Install and enable the Pedelec Chrome Extension.
2. Start the Pedelec Desktop App.
3. Confirm Chrome Native Messaging is registered from the Desktop App.
4. Configure a default provider and model in Desktop App Settings, or ensure the default provider is available.
5. Open the dev URL in Chrome and approve the origin in the extension popup when prompted.

## Test flow

Type a request such as `drop five blue circles and two pink triangles`, then press Enter. The input is sent to Pedelec, and shapes are created only when the agent returns a valid `spawn_basic_shapes` frontend tool call.

The diamond toolbar button drops a local demo batch for rendering/physics checks. It is not the formal natural-language flow.

## Limits

- Supported shapes: circle, square, rectangle, triangle, pentagon, hexagon, star, capsule.
- No freeform drawing, 3D, dragging, or object editor in this first version.
- Commands are clamped to 12 shapes per item and 36 shapes per command.
- The world keeps at most 160 active physics objects and removes old/offscreen objects.
