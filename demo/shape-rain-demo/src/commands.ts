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

export type Point = { x: number; y: number };
export type ClosedPolygonPreset = "heart" | "shield" | "droplet";

export type SpawnClosedPolygonItem = {
  kind: "closedPolygon";
  preset?: ClosedPolygonPreset;
  name?: string;
  outer: Point[];
  holes: Point[][];
  count: number;
  color: string;
  size: number;
  xHint?: number;
};

export type ShapeSpawnItem = SpawnBasicShapeItem | SpawnClosedPolygonItem;

export type SpawnClosedPolygonsResult = {
  success: boolean;
  spawned: number;
  normalizedItems: SpawnClosedPolygonItem[];
  ignored: Array<{ index: number; reason: string; value?: unknown }>;
  limit: {
    maxItems: number;
    maxPerCommand: number;
    maxPerItem: number;
    minPointsPerContour: number;
    maxPointsPerContour: number;
    minSize: number;
    maxSize: number;
  };
  error?: {
    code: string;
    message: string;
  };
};

const MAX_PER_ITEM = 12;
const MAX_PER_COMMAND = 36;
const MIN_SIZE = 8;
const MAX_SIZE = 40;
const CLOSED_MAX_ITEMS = 6;
const CLOSED_MAX_PER_ITEM = 8;
const CLOSED_MAX_PER_COMMAND = 24;
const CLOSED_MIN_SIZE = 28;
const CLOSED_MAX_SIZE = 96;
const MIN_POINTS_PER_CONTOUR = 3;
const MAX_POINTS_PER_CONTOUR = 64;
const MIN_CONTOUR_AREA = 120;

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
  amber: "#ffbf61",
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

export function normalizeSpawnClosedPolygonsCommand(args: unknown, mode: RenderMode = "3d"): SpawnClosedPolygonsResult {
  const ignored: SpawnClosedPolygonsResult["ignored"] = [];

  if (!args || typeof args !== "object" || Array.isArray(args)) {
    return invalidClosedPolygonResult("INVALID_ARGS", "Tool args must be an object with an items array.");
  }

  const rawItems = (args as { items?: unknown }).items;
  if (!Array.isArray(rawItems)) {
    return invalidClosedPolygonResult("INVALID_ITEMS", "Tool args must include an items array.");
  }

  const normalizedItems: SpawnClosedPolygonItem[] = [];
  let remaining = CLOSED_MAX_PER_COMMAND;

  rawItems.slice(0, CLOSED_MAX_ITEMS).forEach((rawItem, index) => {
    if (remaining <= 0) {
      ignored.push({ index, reason: "command limit reached", value: summarizeValue(rawItem) });
      return;
    }

    if (!rawItem || typeof rawItem !== "object" || Array.isArray(rawItem)) {
      ignored.push({ index, reason: "item must be an object", value: rawItem });
      return;
    }

    const item = rawItem as Record<string, unknown>;
    if (item.preset !== undefined && item.outer !== undefined) {
      ignored.push({ index, reason: "provide either preset or outer, not both", value: summarizeValue(item) });
      return;
    }

    const preset = normalizePreset(item.preset);
    const contourSource = preset ? presetContours[preset] : { outer: item.outer, holes: item.holes };
    if (item.preset !== undefined && !preset) {
      ignored.push({ index, reason: "unsupported preset", value: item.preset });
      return;
    }

    const normalizedContour = normalizePolygonContours(contourSource.outer, contourSource.holes);
    if (!normalizedContour.ok) {
      ignored.push({ index, reason: normalizedContour.reason, value: summarizeValue(item) });
      return;
    }

    const count = clamp(toNumber(item.count, 1), 1, Math.min(CLOSED_MAX_PER_ITEM, remaining));
    const size = normalizeClosedPolygonSize(item.size, mode);
    const xHint = normalizeXHint(item.x ?? item.xHint ?? item.spawnX);
    const rawColor = item.color ?? item.style;
    const color =
      typeof rawColor === "string" && rawColor.trim().length > 0
        ? normalizeColor(rawColor)
        : randomCandyColor(normalizedItems.at(-1)?.color);

    normalizedItems.push({
      kind: "closedPolygon",
      ...(preset ? { preset } : {}),
      ...(typeof item.name === "string" && item.name.trim() ? { name: item.name.trim().slice(0, 48) } : {}),
      outer: normalizedContour.outer,
      holes: normalizedContour.holes,
      count,
      color,
      size,
      ...(xHint === undefined ? {} : { xHint }),
    });
    remaining -= count;
  });

  if (rawItems.length > CLOSED_MAX_ITEMS) {
    for (let index = CLOSED_MAX_ITEMS; index < rawItems.length; index += 1) {
      ignored.push({ index, reason: "item limit reached", value: summarizeValue(rawItems[index]) });
    }
  }

  return {
    success: normalizedItems.length > 0,
    spawned: normalizedItems.reduce((total, item) => total + item.count, 0),
    normalizedItems,
    ignored,
    limit: closedPolygonLimit(),
    ...(normalizedItems.length > 0
      ? {}
      : { error: { code: "NO_VALID_ITEMS", message: "No valid closed polygon items were provided." } }),
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

function invalidClosedPolygonResult(code: string, message: string): SpawnClosedPolygonsResult {
  return {
    success: false,
    spawned: 0,
    normalizedItems: [],
    ignored: [],
    limit: closedPolygonLimit(),
    error: { code, message },
  };
}

function closedPolygonLimit(): SpawnClosedPolygonsResult["limit"] {
  return {
    maxItems: CLOSED_MAX_ITEMS,
    maxPerCommand: CLOSED_MAX_PER_COMMAND,
    maxPerItem: CLOSED_MAX_PER_ITEM,
    minPointsPerContour: MIN_POINTS_PER_CONTOUR,
    maxPointsPerContour: MAX_POINTS_PER_CONTOUR,
    minSize: CLOSED_MIN_SIZE,
    maxSize: CLOSED_MAX_SIZE,
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
    else if (named === "medium") size = 28;
    else if (named === "large") size = 40;
    else {
      const parsed = Number(named);
      size = Number.isFinite(parsed) ? clamp(parsed, MIN_SIZE, MAX_SIZE) : clamp(toNumber(value, 12), MIN_SIZE, MAX_SIZE);
    }
  } else {
    size = clamp(toNumber(value, 18), MIN_SIZE, MAX_SIZE);
  }

  return mode === "2d" ? size * 4 : size;
}

function normalizeClosedPolygonSize(value: unknown, mode: RenderMode = "3d"): number {
  let size: number;
  if (typeof value === "string") {
    const named = value.trim().toLowerCase();
    if (named === "small") size = 36;
    else if (named === "large") size = 84;
    else if (named === "medium") size = 64;
    else size = clamp(toNumber(value, 64), CLOSED_MIN_SIZE, CLOSED_MAX_SIZE);
  } else {
    size = clamp(toNumber(value, 64), CLOSED_MIN_SIZE, CLOSED_MAX_SIZE);
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

function normalizePreset(value: unknown): ClosedPolygonPreset | null {
  if (typeof value !== "string") return null;
  const normalized = value.trim().toLowerCase();
  if (normalized === "heart" || normalized === "shield" || normalized === "droplet") return normalized;
  return null;
}

function normalizePolygonContours(
  rawOuter: unknown,
  rawHoles: unknown,
): { ok: true; outer: Point[]; holes: Point[][] } | { ok: false; reason: string } {
  const outer = normalizeContour(rawOuter);
  if (!outer.ok) return { ok: false, reason: `outer ${outer.reason}` };
  if (Math.abs(signedArea(outer.points)) < MIN_CONTOUR_AREA) return { ok: false, reason: "outer area is too small" };
  if (hasSelfIntersection(outer.points)) return { ok: false, reason: "outer self-intersects" };

  if (rawHoles === undefined) return { ok: true, outer: normalizeWinding(outer.points, false), holes: [] };
  if (!Array.isArray(rawHoles)) return { ok: false, reason: "holes must be an array of contours" };

  const holes: Point[][] = [];
  for (let index = 0; index < rawHoles.length; index += 1) {
    const hole = normalizeContour(rawHoles[index]);
    if (!hole.ok) return { ok: false, reason: `hole ${index} ${hole.reason}` };
    if (Math.abs(signedArea(hole.points)) < MIN_CONTOUR_AREA) return { ok: false, reason: `hole ${index} area is too small` };
    if (hasSelfIntersection(hole.points)) return { ok: false, reason: `hole ${index} self-intersects` };
    if (!hole.points.every((point) => pointInPolygon(point, outer.points))) return { ok: false, reason: `hole ${index} is outside outer` };
    for (const previous of holes) {
      if (contoursIntersect(hole.points, previous) || hole.points.some((point) => pointInPolygon(point, previous))) {
        return { ok: false, reason: `hole ${index} overlaps another hole` };
      }
    }
    holes.push(normalizeWinding(hole.points, true));
  }

  return { ok: true, outer: normalizeWinding(outer.points, false), holes };
}

function normalizeContour(rawContour: unknown): { ok: true; points: Point[] } | { ok: false; reason: string } {
  if (!Array.isArray(rawContour)) return { ok: false, reason: "must be a point array" };
  if (rawContour.length < MIN_POINTS_PER_CONTOUR) return { ok: false, reason: "must contain at least 3 points" };
  if (rawContour.length > MAX_POINTS_PER_CONTOUR) return { ok: false, reason: "exceeds 64 points" };

  const points: Point[] = [];
  for (const rawPoint of rawContour) {
    if (!rawPoint || typeof rawPoint !== "object" || Array.isArray(rawPoint)) return { ok: false, reason: "points must be objects" };
    const point = rawPoint as Record<string, unknown>;
    const x = point.x;
    const y = point.y;
    if (typeof x !== "number" || typeof y !== "number" || !Number.isInteger(x) || !Number.isInteger(y)) {
      return { ok: false, reason: "points require integer x and y" };
    }
    if (x < -100 || x > 100 || y < -100 || y > 100) {
      return { ok: false, reason: "points must be in the -100..100 grid" };
    }
    points.push({ x, y });
  }

  if (samePoint(points[0], points[points.length - 1])) return { ok: false, reason: "must not repeat the first point as the final point" };
  if (new Set(points.map((point) => `${point.x},${point.y}`)).size < MIN_POINTS_PER_CONTOUR) {
    return { ok: false, reason: "must contain at least 3 unique points" };
  }

  return { ok: true, points };
}

function normalizeWinding(points: Point[], clockwise: boolean): Point[] {
  const isClockwise = signedArea(points) > 0;
  return isClockwise === clockwise ? points : [...points].reverse();
}

function signedArea(points: Point[]): number {
  return points.reduce((total, point, index) => {
    const next = points[(index + 1) % points.length];
    return total + (point.x * next.y - next.x * point.y);
  }, 0) / 2;
}

function hasSelfIntersection(points: Point[]): boolean {
  for (let a = 0; a < points.length; a += 1) {
    const aNext = (a + 1) % points.length;
    for (let b = a + 1; b < points.length; b += 1) {
      const bNext = (b + 1) % points.length;
      if (a === b || aNext === b || bNext === a) continue;
      if (segmentsIntersect(points[a], points[aNext], points[b], points[bNext])) return true;
    }
  }
  return false;
}

function contoursIntersect(first: Point[], second: Point[]): boolean {
  return first.some((from, firstIndex) => {
    const to = first[(firstIndex + 1) % first.length];
    return second.some((otherFrom, secondIndex) => segmentsIntersect(from, to, otherFrom, second[(secondIndex + 1) % second.length]));
  });
}

function segmentsIntersect(a: Point, b: Point, c: Point, d: Point): boolean {
  const abC = orientation(a, b, c);
  const abD = orientation(a, b, d);
  const cdA = orientation(c, d, a);
  const cdB = orientation(c, d, b);

  if (abC === 0 && onSegment(a, c, b)) return true;
  if (abD === 0 && onSegment(a, d, b)) return true;
  if (cdA === 0 && onSegment(c, a, d)) return true;
  if (cdB === 0 && onSegment(c, b, d)) return true;
  return (abC > 0) !== (abD > 0) && (cdA > 0) !== (cdB > 0);
}

function orientation(a: Point, b: Point, c: Point): number {
  const value = (b.y - a.y) * (c.x - b.x) - (b.x - a.x) * (c.y - b.y);
  return Math.abs(value) < 1e-9 ? 0 : value;
}

function onSegment(a: Point, b: Point, c: Point): boolean {
  return b.x >= Math.min(a.x, c.x) && b.x <= Math.max(a.x, c.x) && b.y >= Math.min(a.y, c.y) && b.y <= Math.max(a.y, c.y);
}

function pointInPolygon(point: Point, polygon: Point[]): boolean {
  let inside = false;
  for (let index = 0, previous = polygon.length - 1; index < polygon.length; previous = index, index += 1) {
    const currentPoint = polygon[index];
    const previousPoint = polygon[previous];
    const intersects =
      currentPoint.y > point.y !== previousPoint.y > point.y &&
      point.x < ((previousPoint.x - currentPoint.x) * (point.y - currentPoint.y)) / (previousPoint.y - currentPoint.y) + currentPoint.x;
    if (intersects) inside = !inside;
  }
  return inside;
}

function samePoint(first: Point, second: Point): boolean {
  return first.x === second.x && first.y === second.y;
}

function summarizeValue(value: unknown): unknown {
  if (!value || typeof value !== "object") return value;
  const item = value as Record<string, unknown>;
  return {
    preset: item.preset,
    name: item.name,
    outerPoints: Array.isArray(item.outer) ? item.outer.length : undefined,
    holes: Array.isArray(item.holes) ? item.holes.length : undefined,
    count: item.count,
  };
}

const presetContours: Record<ClosedPolygonPreset, { outer: Point[]; holes?: Point[][] }> = {
  heart: {
    outer: [
      { x: 0, y: 88 },
      { x: -76, y: 22 },
      { x: -96, y: -30 },
      { x: -72, y: -74 },
      { x: -30, y: -82 },
      { x: 0, y: -52 },
      { x: 30, y: -82 },
      { x: 72, y: -74 },
      { x: 96, y: -30 },
      { x: 76, y: 22 },
    ],
  },
  shield: {
    outer: [
      { x: 0, y: 100 },
      { x: -72, y: 54 },
      { x: -86, y: -66 },
      { x: 0, y: -100 },
      { x: 86, y: -66 },
      { x: 72, y: 54 },
    ],
  },
  droplet: {
    outer: [
      { x: 0, y: -100 },
      { x: 52, y: -28 },
      { x: 76, y: 22 },
      { x: 54, y: 76 },
      { x: 0, y: 100 },
      { x: -54, y: 76 },
      { x: -76, y: 22 },
      { x: -52, y: -28 },
    ],
  },
};

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
