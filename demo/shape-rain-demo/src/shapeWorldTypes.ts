import type { SpawnBasicShapeItem } from "./commands";

export type RenderMode = "2d" | "3d";

export type ShapeWorldLike = {
  mount(container: HTMLElement): Promise<void>;
  spawn(items: SpawnBasicShapeItem[]): number;
  clearObjects(): void;
  destroy(): void;
};
