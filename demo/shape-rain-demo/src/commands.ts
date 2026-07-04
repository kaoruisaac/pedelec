import type { RenderMode } from "./shapeWorldTypes";

export const SUPPORTED_SHAPES = [
  "circle",
  "square",
  "rectangle",
  "triangle",
  "pentagon",
  "hexagon",
  "star",
  "capsule",
] as const;

export type ShapeKind = (typeof SUPPORTED_SHAPES)[number];
export type ShapeSize = "small" | "medium" | "large" | number;

export type SpawnBasicShapeItem = {
  shape: ShapeKind;
  count: number;
  color: string;
  size: number;
  xHint?: number;
};

export type SpawnBasicShapesCommand = {
  items: SpawnBasicShapeItem[];
};

export type SpawnBasicShapesResult = {
  success: boolean;
  spawned: number;
  normalizedItems: SpawnBasicShapeItem[];
  ignored: Array<{ index: number; reason: string; value?: unknown }>;
  limit: {
    maxPerCommand: number;
    maxPerItem: number;
  };
  error?: {
    code: string;
    message: string;
  };
};

const MAX_PER_ITEM = 12;
const MAX_PER_COMMAND = 36;
const MIN_SIZE = 12;
const MAX_SIZE = 86;

const candyPalette = [
  "#90c4ff", // blue
  "#66e0ff", // sky
  "#ff94d4", // pink
  "#ffea70", // yellow
  "#80f0a0", // green
  "#70ecd8", // mint
  "#b888ff", // purple
  "#ffb870", // orange
];

const namedColors: Record<string, string> = {
  blue: "#90c4ff",
  sky: "#66e0ff",
  pink: "#ff94d4",
  rose: "#ffa8dc",
  red: "#ff98b0",
  yellow: "#ffea70",
  gold: "#ffe858",
  green: "#80f0a0",
  mint: "#70ecd8",
  purple: "#b888ff",
  violet: "#c898ff",
  orange: "#ffb870",
  white: "#ffffff",
};

export function normalizeSpawnCommand(args: unknown, mode: RenderMode = "3d"): SpawnBasicShapesResult {
  const ignored: SpawnBasicShapesResult["ignored"] = [];

  if (!args || typeof args !== "object" || Array.isArray(args)) {
    return invalidResult("INVALID_ARGS", "Tool args must be an object with an items array.");
  }

  const rawItems = (args as { items?: unknown }).items;
  if (!Array.isArray(rawItems)) {
    return invalidResult("INVALID_ITEMS", "Tool args must include an items array.");
  }

  const normalizedItems: SpawnBasicShapeItem[] = [];
  let remaining = MAX_PER_COMMAND;

  rawItems.forEach((rawItem, index) => {
    if (remaining <= 0) {
      ignored.push({ index, reason: "command limit reached", value: rawItem });
      return;
    }

    if (!rawItem || typeof rawItem !== "object" || Array.isArray(rawItem)) {
      ignored.push({ index, reason: "item must be an object", value: rawItem });
      return;
    }

    const item = rawItem as Record<string, unknown>;
    const shape = normalizeShape(item.shape);
    if (!shape) {
      ignored.push({ index, reason: "unsupported shape", value: item.shape });
      return;
    }

    const count = clamp(toNumber(item.count, 1), 1, Math.min(MAX_PER_ITEM, remaining));
    const size = normalizeSize(item.size, mode);
    const xHint = normalizeXHint(item.x ?? item.xHint ?? item.spawnX);
    const xHintProps = xHint === undefined ? {} : { xHint };

    const rawColor = item.color ?? item.style;
    if (typeof rawColor === "string" && rawColor.trim().length > 0) {
      normalizedItems.push({ shape, count, color: normalizeColor(rawColor), size, ...xHintProps });
    } else {
      // 未指定顏色時，每個物件各自抽一個糖果色
      let previous: string | undefined;
      for (let unit = 0; unit < count; unit += 1) {
        previous = randomCandyColor(previous);
        normalizedItems.push({ shape, count: 1, color: previous, size, ...xHintProps });
      }
    }
    remaining -= count;
  });

  return {
    success: normalizedItems.length > 0,
    spawned: normalizedItems.reduce((total, item) => total + item.count, 0),
    normalizedItems,
    ignored,
    limit: { maxPerCommand: MAX_PER_COMMAND, maxPerItem: MAX_PER_ITEM },
    ...(normalizedItems.length > 0
      ? {}
      : { error: { code: "NO_VALID_ITEMS", message: "No valid shape items were provided." } }),
  };
}

function invalidResult(code: string, message: string): SpawnBasicShapesResult {
  return {
    success: false,
    spawned: 0,
    normalizedItems: [],
    ignored: [],
    limit: { maxPerCommand: MAX_PER_COMMAND, maxPerItem: MAX_PER_ITEM },
    error: { code, message },
  };
}

function normalizeShape(value: unknown): ShapeKind | null {
  if (typeof value !== "string") return null;
  const normalized = value.trim().toLowerCase();
  return SUPPORTED_SHAPES.find((shape) => shape === normalized) ?? null;
}

function normalizeSize(value: unknown, mode: RenderMode = "3d"): number {
  let size: number;
  if (typeof value === "string") {
    const named = value.trim().toLowerCase();
    if (named === "small") size = 12;
    else if (named === "large") size = 72;
    else {
      const parsed = Number(named);
      size = Number.isFinite(parsed) ? clamp(parsed, MIN_SIZE, MAX_SIZE) : clamp(toNumber(value, 18), MIN_SIZE, MAX_SIZE);
    }
  } else {
    size = clamp(toNumber(value, 18), MIN_SIZE, MAX_SIZE);
  }

  return mode === "2d" ? size * 4 : size;
}

function normalizeColor(value: unknown): string {
  if (typeof value !== "string") return "#90c4ff";
  const trimmed = value.trim().toLowerCase();
  if (namedColors[trimmed]) return namedColors[trimmed];
  if (/^#[0-9a-f]{3}$/i.test(trimmed)) {
    return `#${trimmed[1]}${trimmed[1]}${trimmed[2]}${trimmed[2]}${trimmed[3]}${trimmed[3]}`;
  }
  if (/^#[0-9a-f]{6}$/i.test(trimmed)) return trimmed;
  return "#90c4ff";
}

function randomCandyColor(previous?: string): string {
  const pool = previous === undefined ? candyPalette : candyPalette.filter((color) => color !== previous);
  return pool[Math.floor(Math.random() * pool.length)];
}

function normalizeXHint(value: unknown): number | undefined {
  if (value === undefined) return undefined;
  const numeric = toNumber(value, Number.NaN);
  if (!Number.isFinite(numeric)) return undefined;
  return clamp(numeric, 0, 1);
}

function toNumber(value: unknown, fallback: number): number {
  const numeric = typeof value === "number" ? value : typeof value === "string" ? Number(value) : Number.NaN;
  return Number.isFinite(numeric) ? numeric : fallback;
}

function clamp(value: number, min: number, max: number): number {
  return Math.min(max, Math.max(min, value));
}
