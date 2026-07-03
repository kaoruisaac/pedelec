import { Application, Container, Graphics } from "pixi.js";
import Matter from "matter-js";
import type { ShapeKind, SpawnBasicShapeItem } from "./commands";

type ShapeObject = {
  id: number;
  body: Matter.Body;
  view: Container;
  bornAt: number;
};

const WALL_THICKNESS = 120;
const MAX_WORLD_OBJECTS = 160;

export class ShapeWorld {
  private app: Application | null = null;
  private engine: Matter.Engine | null = null;
  private runner: Matter.Runner | null = null;
  private container: HTMLElement | null = null;
  private floor: Matter.Body | null = null;
  private leftWall: Matter.Body | null = null;
  private rightWall: Matter.Body | null = null;
  private objects: ShapeObject[] = [];
  private nextId = 1;
  private resizeObserver: ResizeObserver | null = null;
  private ticker: (() => void) | null = null;

  async mount(container: HTMLElement): Promise<void> {
    this.container = container;
    this.engine = Matter.Engine.create({
      gravity: { x: 0, y: 0.96 },
      positionIterations: 8,
      velocityIterations: 6,
    });
    this.runner = Matter.Runner.create();
    this.app = new Application();

    await this.app.init({
      backgroundAlpha: 0,
      antialias: true,
      autoDensity: true,
      resolution: Math.min(window.devicePixelRatio || 1, 2),
      resizeTo: container,
    });

    this.app.canvas.className = "shape-stage-canvas";
    container.append(this.app.canvas);
    this.updateBounds();
    Matter.Runner.run(this.runner, this.engine);

    this.ticker = () => this.syncViews();
    this.app.ticker.add(this.ticker);
    this.resizeObserver = new ResizeObserver(() => this.updateBounds());
    this.resizeObserver.observe(container);

    this.spawn([
      { shape: "circle", count: 1, color: "#ffd24d", size: 48 },
      { shape: "triangle", count: 1, color: "#8f6be8", size: 56 },
      { shape: "square", count: 1, color: "#4f7df3", size: 54 },
    ]);
  }

  destroy(): void {
    if (this.ticker && this.app) this.app.ticker.remove(this.ticker);
    this.resizeObserver?.disconnect();
    this.resizeObserver = null;
    this.clearObjects();
    if (this.runner && this.engine) Matter.Runner.stop(this.runner);
    if (this.engine) Matter.Engine.clear(this.engine);
    this.app?.destroy(true);
    this.app = null;
    this.engine = null;
    this.runner = null;
    this.container = null;
  }

  spawn(items: SpawnBasicShapeItem[]): number {
    if (!this.app || !this.engine || !this.container) return 0;

    const rect = this.container.getBoundingClientRect();
    let spawned = 0;

    for (const item of items) {
      for (let index = 0; index < item.count; index += 1) {
        const jitter = (Math.random() - 0.5) * Math.min(220, rect.width * 0.22);
        const baseX = item.xHint === undefined ? rect.width * 0.5 : item.xHint * rect.width;
        const x = clamp(baseX + jitter, item.size + 18, rect.width - item.size - 18);
        const y = -item.size * (1.2 + index * 0.9 + Math.random() * 1.2);
        const object = this.createObject(item.shape, item.color, item.size, x, y);
        Matter.Composite.add(this.engine.world, object.body);
        this.app.stage.addChild(object.view);
        this.objects.push(object);
        spawned += 1;
      }
    }

    this.enforceObjectLimit();
    return spawned;
  }

  clearObjects(): void {
    if (!this.engine) return;
    for (const object of this.objects) {
      Matter.Composite.remove(this.engine.world, object.body);
      object.view.destroy({ children: true });
    }
    this.objects = [];
  }

  private createObject(shape: ShapeKind, color: string, size: number, x: number, y: number): ShapeObject {
    const body = makeBody(shape, x, y, size);
    Matter.Body.setAngle(body, (Math.random() - 0.5) * 0.9);
    Matter.Body.setAngularVelocity(body, (Math.random() - 0.5) * 0.08);
    Matter.Body.setVelocity(body, { x: (Math.random() - 0.5) * 2.5, y: Math.random() * 1.2 });

    return {
      id: this.nextId++,
      body,
      view: drawShape(shape, color, size),
      bornAt: Date.now(),
    };
  }

  private updateBounds(): void {
    if (!this.engine || !this.container) return;
    const { width, height } = this.container.getBoundingClientRect();
    const floorY = height + WALL_THICKNESS * 0.5 - 22;
    const wallY = height * 0.5;

    const nextBounds = [
      Matter.Bodies.rectangle(width * 0.5, floorY, width + WALL_THICKNESS * 2, WALL_THICKNESS, {
        isStatic: true,
        label: "floor",
      }),
      Matter.Bodies.rectangle(-WALL_THICKNESS * 0.5, wallY, WALL_THICKNESS, height * 2, {
        isStatic: true,
        label: "left-wall",
      }),
      Matter.Bodies.rectangle(width + WALL_THICKNESS * 0.5, wallY, WALL_THICKNESS, height * 2, {
        isStatic: true,
        label: "right-wall",
      }),
    ];

    const oldBounds = [this.floor, this.leftWall, this.rightWall].filter(Boolean) as Matter.Body[];
    if (oldBounds.length) Matter.Composite.remove(this.engine.world, oldBounds);
    [this.floor, this.leftWall, this.rightWall] = nextBounds;
    Matter.Composite.add(this.engine.world, nextBounds);

    for (const object of this.objects) {
      const x = clamp(object.body.position.x, 20, Math.max(20, width - 20));
      if (x !== object.body.position.x) Matter.Body.setPosition(object.body, { x, y: object.body.position.y });
    }
  }

  private syncViews(): void {
    if (!this.container) return;
    const height = this.container.getBoundingClientRect().height;

    for (const object of this.objects) {
      object.view.position.set(object.body.position.x, object.body.position.y);
      object.view.rotation = object.body.angle;
    }

    const expired = this.objects.filter((object) => object.body.position.y > height + 360);
    for (const object of expired) this.removeObject(object);
  }

  private enforceObjectLimit(): void {
    const overflow = this.objects.length - MAX_WORLD_OBJECTS;
    if (overflow <= 0) return;
    const toRemove = [...this.objects].sort((a, b) => a.bornAt - b.bornAt).slice(0, overflow);
    for (const object of toRemove) this.removeObject(object);
  }

  private removeObject(object: ShapeObject): void {
    if (!this.engine) return;
    Matter.Composite.remove(this.engine.world, object.body);
    object.view.destroy({ children: true });
    this.objects = this.objects.filter((candidate) => candidate.id !== object.id);
  }
}

function makeBody(shape: ShapeKind, x: number, y: number, size: number): Matter.Body {
  const options = {
    friction: 0.62,
    frictionAir: 0.012,
    restitution: 0.28,
    density: 0.0016,
  } satisfies Matter.IChamferableBodyDefinition;

  if (shape === "circle") return Matter.Bodies.circle(x, y, size * 0.5, options);
  if (shape === "rectangle") return Matter.Bodies.rectangle(x, y, size * 1.35, size * 0.78, options);
  if (shape === "capsule") return Matter.Bodies.rectangle(x, y, size * 1.48, size * 0.72, { ...options, chamfer: { radius: size * 0.34 } });
  if (shape === "square") return Matter.Bodies.rectangle(x, y, size, size, options);

  const sides = shape === "triangle" ? 3 : shape === "pentagon" ? 5 : shape === "hexagon" ? 6 : 10;
  return Matter.Bodies.polygon(x, y, sides, size * 0.55, options);
}

function drawShape(shape: ShapeKind, color: string, size: number): Container {
  return drawGlassShape(shape, color, size);
}

function drawGlassShape(shape: ShapeKind, color: string, size: number): Container {
  const root = new Container();
  const baseColor = hexToPixiColor(color);
  const lightEdge = hexToPixiColor(lightenColor(color, 0.5));
  const paleGlow = hexToPixiColor(lightenColor(color, 0.78));

  const softShadow = new Graphics();
  softShadow.position.set(0, size * 0.11);
  drawScaledPath(softShadow, shape, size, 1.04);
  softShadow.fill({ color: baseColor, alpha: 0.13 });

  const floorShadow = new Graphics();
  floorShadow.position.set(size * 0.02, size * 0.18);
  drawScaledPath(floorShadow, shape, size, 0.96);
  floorShadow.fill({ color: 0x64748b, alpha: 0.11 });

  const outerGlow = new Graphics();
  drawScaledPath(outerGlow, shape, size, 1.06);
  outerGlow.stroke({ color: paleGlow, alpha: 0.62, width: Math.max(5, size * 0.1) });

  const body = new Graphics();
  drawPath(body, shape, size);
  body.fill({ color: baseColor, alpha: 0.5 });
  body.stroke({ color: lightEdge, alpha: 0.9, width: Math.max(3, size * 0.06) });

  const innerTint = new Graphics();
  drawScaledPath(innerTint, shape, size, 0.82);
  innerTint.fill({ color: 0xffffff, alpha: 0.1 });

  const innerEdge = new Graphics();
  drawScaledPath(innerEdge, shape, size, 0.82);
  innerEdge.stroke({ color: 0xffffff, alpha: 0.68, width: Math.max(1.5, size * 0.027) });

  const lowerEdge = new Graphics();
  lowerEdge.position.set(size * 0.025, size * 0.035);
  drawScaledPath(lowerEdge, shape, size, 0.91);
  lowerEdge.stroke({ color: baseColor, alpha: 0.38, width: Math.max(2, size * 0.035) });

  const shine = new Graphics();
  drawHighlight(shine, shape, size, lightEdge);

  root.addChild(softShadow, floorShadow, outerGlow, body, innerTint, lowerEdge, innerEdge, shine);
  return root;
}

function drawScaledPath(graphics: Graphics, shape: ShapeKind, size: number, scale: number): void {
  drawPath(graphics, shape, size * scale);
}

function drawPath(graphics: Graphics, shape: ShapeKind, size: number): void {
  const half = size * 0.5;
  if (shape === "circle") {
    graphics.circle(0, 0, half);
    return;
  }
  if (shape === "square") {
    graphics.rect(-half, -half, size, size);
    return;
  }
  if (shape === "rectangle") {
    graphics.roundRect(-size * 0.67, -size * 0.39, size * 1.34, size * 0.78, 8);
    return;
  }
  if (shape === "capsule") {
    graphics.roundRect(-size * 0.74, -size * 0.36, size * 1.48, size * 0.72, size * 0.36);
    return;
  }
  if (shape === "star") {
    polygon(graphics, starPoints(size * 0.58, size * 0.26, 5));
    return;
  }

  const sides = shape === "triangle" ? 3 : shape === "pentagon" ? 5 : 6;
  polygon(graphics, regularPolygonPoints(sides, size * 0.58, shape === "triangle" ? -Math.PI / 2 : -Math.PI / 2));
}

function drawHighlight(graphics: Graphics, shape: ShapeKind, size: number, edgeColor: number): void {
  graphics.alpha = 0.82;
  if (shape === "circle") {
    graphics.arc(-size * 0.08, -size * 0.05, size * 0.34, Math.PI * 1.08, Math.PI * 1.62);
    graphics.stroke({ color: 0xffffff, alpha: 0.82, width: Math.max(2, size * 0.045) });
    graphics.circle(size * 0.18, -size * 0.2, size * 0.06);
    graphics.fill({ color: 0xffffff, alpha: 0.35 });
    return;
  }

  if (shape === "capsule" || shape === "rectangle") {
    graphics.moveTo(-size * 0.43, -size * 0.2);
    graphics.lineTo(size * 0.18, -size * 0.3);
    graphics.stroke({ color: 0xffffff, alpha: 0.78, width: Math.max(2, size * 0.04) });
    graphics.moveTo(-size * 0.5, size * 0.19);
    graphics.lineTo(size * 0.44, size * 0.11);
    graphics.stroke({ color: edgeColor, alpha: 0.44, width: Math.max(1.5, size * 0.028) });
    return;
  }

  if (shape === "star") {
    graphics.moveTo(-size * 0.24, -size * 0.12);
    graphics.lineTo(-size * 0.04, -size * 0.34);
    graphics.lineTo(size * 0.11, -size * 0.11);
  } else {
    graphics.moveTo(-size * 0.27, -size * 0.26);
    graphics.lineTo(size * 0.14, -size * 0.37);
  }
  graphics.stroke({ color: 0xffffff, alpha: 0.78, width: Math.max(2, size * 0.045) });
}

function regularPolygonPoints(sides: number, radius: number, offset = 0): number[] {
  return Array.from({ length: sides }, (_, index) => {
    const angle = offset + (Math.PI * 2 * index) / sides;
    return [Math.cos(angle) * radius, Math.sin(angle) * radius];
  }).flat();
}

function starPoints(outer: number, inner: number, points: number): number[] {
  return Array.from({ length: points * 2 }, (_, index) => {
    const radius = index % 2 === 0 ? outer : inner;
    const angle = -Math.PI / 2 + (Math.PI * index) / points;
    return [Math.cos(angle) * radius, Math.sin(angle) * radius];
  }).flat();
}

function polygon(graphics: Graphics, points: number[]): void {
  graphics.poly(points);
}

function clamp(value: number, min: number, max: number): number {
  return Math.min(max, Math.max(min, value));
}

function hexToPixiColor(hex: string): number {
  return Number.parseInt(hex.replace("#", ""), 16);
}

function lightenColor(hex: string, amount: number): string {
  const { r, g, b } = hexToRgb(hex);
  return rgbToHex(
    Math.round(r + (255 - r) * amount),
    Math.round(g + (255 - g) * amount),
    Math.round(b + (255 - b) * amount),
  );
}

function hexToRgb(hex: string): { r: number; g: number; b: number } {
  const normalized = hex.replace("#", "");
  const value = Number.parseInt(normalized, 16);
  return {
    r: (value >> 16) & 255,
    g: (value >> 8) & 255,
    b: value & 255,
  };
}

function rgbToHex(r: number, g: number, b: number): string {
  return `#${[r, g, b].map((channel) => clamp(channel, 0, 255).toString(16).padStart(2, "0")).join("")}`;
}
