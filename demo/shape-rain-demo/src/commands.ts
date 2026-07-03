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
const MIN_SIZE = 28;
const MAX_SIZE = 86;

const namedColors: Record<string, string> = {
  blue: "#4f7df3",
  sky: "#57b7ff",
  pink: "#f05a87",
  rose: "#ff6d8d",
  red: "#f15d68",
  yellow: "#ffd24d",
  gold: "#f7c94b",
  green: "#65c875",
  mint: "#62d6a3",
  purple: "#8f6be8",
  violet: "#9b6ef3",
  orange: "#ff9f43",
  white: "#ffffff",
};

export function normalizeSpawnCommand(args: unknown): SpawnBasicShapesResult {
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
    const size = normalizeSize(item.size);
    const color = normalizeColor(item.color ?? item.style);
    const xHint = normalizeXHint(item.x ?? item.xHint ?? item.spawnX);

    normalizedItems.push({ shape, count, color, size, ...(xHint === undefined ? {} : { xHint }) });
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

function normalizeSize(value: unknown): number {
  if (typeof value === "string") {
    const named = value.trim().toLowerCase();
    if (named === "small") return 34;
    if (named === "large") return 72;
    const parsed = Number(named);
    if (Number.isFinite(parsed)) return clamp(parsed, MIN_SIZE, MAX_SIZE);
  }

  return clamp(toNumber(value, 52), MIN_SIZE, MAX_SIZE);
}

function normalizeColor(value: unknown): string {
  if (typeof value !== "string") return "#4f7df3";
  const trimmed = value.trim().toLowerCase();
  if (namedColors[trimmed]) return namedColors[trimmed];
  if (/^#[0-9a-f]{3}$/i.test(trimmed)) {
    return `#${trimmed[1]}${trimmed[1]}${trimmed[2]}${trimmed[2]}${trimmed[3]}${trimmed[3]}`;
  }
  if (/^#[0-9a-f]{6}$/i.test(trimmed)) return trimmed;
  return "#4f7df3";
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
