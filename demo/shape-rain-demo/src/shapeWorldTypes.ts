import type { ShapeSpawnItem } from "./commands";

export type RenderMode = "2d" | "3d";

export type ShapeWorldLike = {
  mount(container: HTMLElement): Promise<void>;
  spawn(items: ShapeSpawnItem[]): number;
  clearObjects(): void;
  destroy(): void;
};
