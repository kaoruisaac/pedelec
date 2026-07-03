import { ShapeWorld } from "./shapeWorld";
import { ShapeWorld3D } from "./shapeWorld3D";
import type { RenderMode, ShapeWorldLike } from "./shapeWorldTypes";

export function createShapeWorld(mode: RenderMode): ShapeWorldLike {
  return mode === "2d" ? new ShapeWorld() : new ShapeWorld3D();
}
