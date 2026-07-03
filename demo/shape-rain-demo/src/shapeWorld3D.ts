import * as THREE from "three";
import { RoomEnvironment } from "three/examples/jsm/environments/RoomEnvironment.js";
import RAPIER, { type Collider, type RigidBody, type World } from "@dimforge/rapier3d-compat";
import type { ShapeKind, SpawnBasicShapeItem } from "./commands";
import type { ShapeWorldLike } from "./shapeWorldTypes";

type ShapeObject3D = {
  id: number;
  body: RigidBody;
  mesh: THREE.Object3D;
  bornAt: number;
};

const MAX_WORLD_OBJECTS = 110;
const WALL_THICKNESS = 0.4;
const SHAPE_THICKNESS = 0.2;
const SLOT_HALF_DEPTH = SHAPE_THICKNESS * 0.5 + 0.06;
const FALL_LIMIT_PADDING = 5;

export class ShapeWorld3D implements ShapeWorldLike {
  private container: HTMLElement | null = null;
  private renderer: THREE.WebGLRenderer | null = null;
  private scene: THREE.Scene | null = null;
  private camera: THREE.OrthographicCamera | null = null;
  private physics: World | null = null;
  private objects: ShapeObject3D[] = [];
  private staticColliders: Collider[] = [];
  private resizeObserver: ResizeObserver | null = null;
  private frameId = 0;
  private destroyed = false;
  private nextId = 1;
  private viewWidth = 10;
  private viewHeight = 8;
  private lastStep = 0;

  async mount(container: HTMLElement): Promise<void> {
    this.destroyed = false;
    this.container = container;
    await RAPIER.init();
    if (this.destroyed) return;

    this.scene = new THREE.Scene();
    this.camera = new THREE.OrthographicCamera(-5, 5, 4, -4, 0.1, 80);
    this.camera.position.set(0, 0, 13);
    this.camera.lookAt(0, 0, 0);

    this.renderer = new THREE.WebGLRenderer({
      alpha: true,
      antialias: true,
      powerPreference: "high-performance",
    });
    this.renderer.setClearColor(0xffffff, 0);
    this.renderer.setPixelRatio(Math.min(window.devicePixelRatio || 1, 1.75));
    this.renderer.outputColorSpace = THREE.SRGBColorSpace;
    this.renderer.toneMapping = THREE.ACESFilmicToneMapping;
    this.renderer.toneMappingExposure = 1.12;
    this.renderer.shadowMap.enabled = true;
    this.renderer.shadowMap.type = THREE.PCFShadowMap;
    this.renderer.domElement.className = "shape-stage-canvas shape-stage-canvas-3d";
    container.append(this.renderer.domElement);

    this.physics = new RAPIER.World({ x: 0, y: -9.35, z: 0 });
    this.setupScene();
    this.resize();
    this.rebuildBounds();

    this.resizeObserver = new ResizeObserver(() => {
      this.resize();
      this.rebuildBounds();
    });
    this.resizeObserver.observe(container);
    this.lastStep = performance.now();
    this.frameId = requestAnimationFrame((time) => this.animate(time));
  }

  spawn(items: SpawnBasicShapeItem[]): number {
    if (!this.scene || !this.physics) return 0;
    let spawned = 0;

    for (const item of items) {
      for (let index = 0; index < item.count; index += 1) {
        const radius = sizeToWorld(item.size);
        const halfSpan = Math.max(0, this.viewWidth * 0.5 - radius);
        const baseX =
          item.xHint === undefined
            ? randomRange(-halfSpan, halfSpan)
            : (item.xHint - 0.5) * (this.viewWidth - radius * 2) + randomRange(-0.6, 0.6);
        const x = clamp(baseX, -halfSpan, halfSpan);
        const y = this.viewHeight * 0.5 + radius * 2 + index * radius * 1.6 + randomRange(0, radius * 1.4);
        const body = this.physics.createRigidBody(
          RAPIER.RigidBodyDesc.dynamic()
            .setTranslation(x, y, 0)
            .setRotation(zOnlyQuaternion())
            .setLinvel(randomRange(-0.5, 0.5), randomRange(-0.2, 0.3), 0)
            .setAngvel({ x: 0, y: 0, z: randomRange(-2.4, 2.4) })
            .setLinearDamping(0.14)
            .setAngularDamping(0.22)
            .enabledTranslations(true, true, false)
            .enabledRotations(false, false, true)
            .setCanSleep(true),
        );
        this.physics.createCollider(makeCollider(item.shape, radius), body);

        const mesh = createShapeMesh(item.shape, item.color, radius);
        mesh.position.set(x, y, 0);
        this.scene.add(mesh);
        this.objects.push({ id: this.nextId++, body, mesh, bornAt: performance.now() });
        spawned += 1;
      }
    }

    this.enforceObjectLimit();
    return spawned;
  }

  clearObjects(): void {
    if (!this.physics) return;
    for (const object of this.objects) this.removeObject(object);
    this.objects = [];
  }

  destroy(): void {
    this.destroyed = true;
    if (this.frameId) cancelAnimationFrame(this.frameId);
    this.frameId = 0;
    this.resizeObserver?.disconnect();
    this.resizeObserver = null;
    this.clearObjects();
    this.staticColliders = [];
    if (this.scene) {
      this.scene.environment?.dispose();
      this.scene.environment = null;
      for (const child of [...this.scene.children]) {
        this.scene.remove(child);
        disposeObject(child);
      }
    }
    this.physics = null;
    this.renderer?.domElement.remove();
    this.renderer?.dispose();
    this.renderer = null;
    this.scene = null;
    this.camera = null;
    this.container = null;
  }

  private setupScene(): void {
    if (!this.scene || !this.renderer) return;

    const pmrem = new THREE.PMREMGenerator(this.renderer);
    this.scene.environment = pmrem.fromScene(new RoomEnvironment(), 0.6).texture;
    pmrem.dispose();

    this.scene.add(new THREE.HemisphereLight(0xffffff, 0xd7e3ff, 1.8));

    const key = new THREE.DirectionalLight(0xffffff, 1.5);
    key.position.set(-4, 8, 8);
    key.castShadow = true;
    key.shadow.mapSize.set(1024, 1024);
    key.shadow.camera.near = 0.5;
    key.shadow.camera.far = 28;
    key.shadow.camera.left = -8;
    key.shadow.camera.right = 8;
    key.shadow.camera.top = 8;
    key.shadow.camera.bottom = -8;
    this.scene.add(key);

    const fill = new THREE.DirectionalLight(0xdceeff, 0.7);
    fill.position.set(5, 3, 6);
    this.scene.add(fill);

    const floor = new THREE.Mesh(
      new THREE.PlaneGeometry(32, 7),
      new THREE.ShadowMaterial({ color: 0x8aa0c2, opacity: 0.16, transparent: true }),
    );
    floor.name = "soft-contact-shadow";
    floor.receiveShadow = true;
    floor.rotation.x = -Math.PI / 2;
    floor.position.set(0, -3.82, 0.16);
    this.scene.add(floor);
  }

  private resize(): void {
    if (!this.container || !this.renderer || !this.camera) return;
    const rect = this.container.getBoundingClientRect();
    const width = Math.max(1, rect.width);
    const height = Math.max(1, rect.height);
    const aspect = width / height;
    this.viewHeight = 8;
    this.viewWidth = this.viewHeight * aspect;
    this.camera.left = -this.viewWidth * 0.5;
    this.camera.right = this.viewWidth * 0.5;
    this.camera.top = this.viewHeight * 0.5;
    this.camera.bottom = -this.viewHeight * 0.5;
    this.camera.updateProjectionMatrix();
    this.renderer.setSize(width, height, false);
  }

  private rebuildBounds(): void {
    if (!this.physics) return;
    for (const collider of this.staticColliders) this.physics.removeCollider(collider, false);
    const floorHalfHeight = 0.9;
    const floorY = -this.viewHeight * 0.5 + 1.1 - floorHalfHeight;
    const leftX = -this.viewWidth * 0.5 - WALL_THICKNESS * 0.5;
    const rightX = this.viewWidth * 0.5 + WALL_THICKNESS * 0.5;
    this.staticColliders = [
      this.physics.createCollider(
        RAPIER.ColliderDesc.cuboid(this.viewWidth * 0.5 + WALL_THICKNESS, floorHalfHeight, SLOT_HALF_DEPTH).setTranslation(0, floorY, 0),
      ),
      this.physics.createCollider(
        RAPIER.ColliderDesc.cuboid(WALL_THICKNESS * 0.5, this.viewHeight, SLOT_HALF_DEPTH).setTranslation(leftX, 0, 0),
      ),
      this.physics.createCollider(
        RAPIER.ColliderDesc.cuboid(WALL_THICKNESS * 0.5, this.viewHeight, SLOT_HALF_DEPTH).setTranslation(rightX, 0, 0),
      ),
      this.physics.createCollider(
        RAPIER.ColliderDesc.cuboid(this.viewWidth, this.viewHeight, WALL_THICKNESS * 0.5).setTranslation(
          0,
          0,
          -(SLOT_HALF_DEPTH + WALL_THICKNESS * 0.5),
        ),
      ),
      this.physics.createCollider(
        RAPIER.ColliderDesc.cuboid(this.viewWidth, this.viewHeight, WALL_THICKNESS * 0.5).setTranslation(
          0,
          0,
          SLOT_HALF_DEPTH + WALL_THICKNESS * 0.5,
        ),
      ),
    ];
  }

  private animate(time: number): void {
    if (this.destroyed || !this.renderer || !this.scene || !this.camera || !this.physics) return;
    const delta = Math.min(1 / 30, Math.max(1 / 120, (time - this.lastStep) / 1000));
    this.lastStep = time;
    this.physics.timestep = delta;
    this.physics.step();

    for (const object of this.objects) {
      const translation = object.body.translation();
      const rotation = object.body.rotation();
      object.mesh.position.set(translation.x, translation.y, translation.z);
      object.mesh.quaternion.set(rotation.x, rotation.y, rotation.z, rotation.w);
    }

    const fallLimit = -this.viewHeight * 0.5 - FALL_LIMIT_PADDING;
    for (const object of this.objects.filter((candidate) => candidate.body.translation().y < fallLimit)) {
      this.removeObject(object);
    }

    this.renderer.render(this.scene, this.camera);
    this.frameId = requestAnimationFrame((nextTime) => this.animate(nextTime));
  }

  private enforceObjectLimit(): void {
    const overflow = this.objects.length - MAX_WORLD_OBJECTS;
    if (overflow <= 0) return;
    const toRemove = [...this.objects].sort((a, b) => a.bornAt - b.bornAt).slice(0, overflow);
    for (const object of toRemove) this.removeObject(object);
  }

  private removeObject(object: ShapeObject3D): void {
    if (!this.physics) return;
    this.physics.removeRigidBody(object.body);
    object.mesh.removeFromParent();
    disposeObject(object.mesh);
    this.objects = this.objects.filter((candidate) => candidate.id !== object.id);
  }
}

function createShapeMesh(shape: ShapeKind, color: string, radius: number): THREE.Object3D {
  const group = new THREE.Group();
  const geometry = makeGeometry(shape, radius);
  geometry.computeVertexNormals();

  const material = new THREE.MeshPhysicalMaterial({
    color: new THREE.Color(color).lerp(new THREE.Color(0xffffff), 0.08), // 基礎色，混入 8% 白使色彩更亮
    roughness: 0.32, // 表面粗糙度，略帶霧面感
    metalness: 0, // 金屬度，0 為非金屬材質
    transmission: 0.96, // 透光率，接近全透明玻璃
    thickness: SHAPE_THICKNESS * 2.2, // 玻璃厚度，影響折射與內部散射
    attenuationDistance: 1.6, // 光線在材質內的衰減距離
    attenuationColor: new THREE.Color(color), // 光線被吸收時呈現的色調
    clearcoat: 0.55, // 清漆層強度，模擬表面光澤
    clearcoatRoughness: 0.4, // 清漆層粗糙度
    ior: 1.45, // 折射率（Index of Refraction），類似玻璃
    envMapIntensity: 0.7, // 環境貼圖反射強度
    specularIntensity: 0.6, // 高光反射強度
  });
  const mesh = new THREE.Mesh(geometry, material);
  mesh.castShadow = true;
  mesh.receiveShadow = true;
  group.add(mesh);

  const rim = new THREE.Mesh(
    geometry.clone(),
    new THREE.MeshBasicMaterial({
      color: new THREE.Color(color).lerp(new THREE.Color(0xffffff), 0.72), // 邊緣光色，混入 72% 白使輪廓更亮
      transparent: true, // 啟用透明度
      opacity: 0.2, // 整體不透明度，低值呈現柔和光暈
      blending: THREE.AdditiveBlending, // 加法混合，疊加出發光效果
      side: THREE.FrontSide, // 只渲染背面，形成外圈 rim light
      depthWrite: false, // 不寫入深度緩衝，避免半透明邊緣遮擋主體
    }),
  );
  rim.scale.setScalar(1.035);
  group.add(rim);
  return group;
}

function makeGeometry(shape: ShapeKind, radius: number): THREE.BufferGeometry {
  if (shape === "circle") {
    const disc = new THREE.Shape();
    disc.absarc(0, 0, radius, 0, Math.PI * 2, false);
    return extrudeShape(disc);
  }
  if (shape === "square") return extrudeRoundedRect(radius * 1.7, radius * 1.7, radius * 0.16);
  if (shape === "rectangle") return extrudeRoundedRect(radius * 2.3, radius * 1.4, radius * 0.28);
  if (shape === "capsule") return makeCapsuleGeometry(radius);
  if (shape === "star") return extrudePoints(starPoints(radius * 1.05, radius * 0.5, 5));
  const sides = shape === "triangle" ? 3 : shape === "pentagon" ? 5 : 6;
  return extrudePoints(regularPolygonPoints(sides, radius, -Math.PI / 2));
}

function makeCapsuleGeometry(radius: number): THREE.BufferGeometry {
  const shape = new THREE.Shape();
  const width = radius * 1.82;
  const height = radius * 0.94;
  const end = height * 0.5;
  shape.absarc(-width * 0.5 + end, 0, end, Math.PI / 2, Math.PI * 1.5, false);
  shape.absarc(width * 0.5 - end, 0, end, Math.PI * 1.5, Math.PI / 2, false);
  shape.closePath();
  return extrudeShape(shape);
}

function extrudeRoundedRect(width: number, height: number, cornerRadius: number): THREE.BufferGeometry {
  const halfWidth = width * 0.5;
  const halfHeight = height * 0.5;
  const r = Math.min(cornerRadius, halfWidth, halfHeight);
  const shape = new THREE.Shape();
  shape.moveTo(-halfWidth + r, -halfHeight);
  shape.lineTo(halfWidth - r, -halfHeight);
  shape.quadraticCurveTo(halfWidth, -halfHeight, halfWidth, -halfHeight + r);
  shape.lineTo(halfWidth, halfHeight - r);
  shape.quadraticCurveTo(halfWidth, halfHeight, halfWidth - r, halfHeight);
  shape.lineTo(-halfWidth + r, halfHeight);
  shape.quadraticCurveTo(-halfWidth, halfHeight, -halfWidth, halfHeight - r);
  shape.lineTo(-halfWidth, -halfHeight + r);
  shape.quadraticCurveTo(-halfWidth, -halfHeight, -halfWidth + r, -halfHeight);
  shape.closePath();
  return extrudeShape(shape);
}

function extrudePoints(points: Array<[number, number]>): THREE.BufferGeometry {
  const shape = new THREE.Shape();
  points.forEach(([x, y], index) => {
    if (index === 0) shape.moveTo(x, y);
    else shape.lineTo(x, y);
  });
  shape.closePath();
  return extrudeShape(shape);
}

function extrudeShape(shape: THREE.Shape): THREE.BufferGeometry {
  const bevel = SHAPE_THICKNESS * 0.1;
  const geometry = new THREE.ExtrudeGeometry(shape, {
    depth: SHAPE_THICKNESS - bevel * 2,
    bevelEnabled: true,
    bevelSegments: 3,
    bevelSize: bevel * 0.9,
    bevelThickness: bevel,
    curveSegments: 24,
    steps: 5,
  });
  geometry.center();
  return geometry;
}

function makeCollider(shape: ShapeKind, radius: number): RAPIER.ColliderDesc {
  if (shape === "circle") {
    return withCandyPhysics(
      RAPIER.ColliderDesc.cylinder(SHAPE_THICKNESS * 0.5, radius).setRotation({
        x: Math.SQRT1_2,
        y: 0,
        z: 0,
        w: Math.SQRT1_2,
      }),
    );
  }

  const borderRadius = Math.min(0.06, SHAPE_THICKNESS * 0.25);
  const halfZ = Math.max(0.01, SHAPE_THICKNESS * 0.5 - borderRadius);
  if (shape === "square") return withCandyPhysics(RAPIER.ColliderDesc.roundCuboid(radius * 0.85, radius * 0.85, halfZ, borderRadius));
  if (shape === "rectangle") return withCandyPhysics(RAPIER.ColliderDesc.roundCuboid(radius * 1.15, radius * 0.7, halfZ, borderRadius));
  if (shape === "capsule") return withCandyPhysics(RAPIER.ColliderDesc.roundCuboid(radius * 0.91, radius * 0.47, halfZ, borderRadius));
  if (shape === "star") return convexHullCollider(starPoints(radius * 1.05, radius * 0.5, 5), radius * 0.95, radius * 0.95);

  const sides = shape === "triangle" ? 3 : shape === "pentagon" ? 5 : 6;
  return convexHullCollider(regularPolygonPoints(sides, radius, -Math.PI / 2), radius, radius);
}

function withCandyPhysics(collider: RAPIER.ColliderDesc): RAPIER.ColliderDesc {
  return collider.setDensity(0.72).setFriction(0.75).setRestitution(0.15);
}

function convexHullCollider(points: Array<[number, number]>, fallbackHalfX: number, fallbackHalfY: number): RAPIER.ColliderDesc {
  const halfZ = SHAPE_THICKNESS * 0.5;
  const vertices = new Float32Array(points.length * 2 * 3);

  points.forEach(([x, y], index) => {
    const front = index * 3;
    vertices[front] = x;
    vertices[front + 1] = y;
    vertices[front + 2] = halfZ;

    const back = (points.length + index) * 3;
    vertices[back] = x;
    vertices[back + 1] = y;
    vertices[back + 2] = -halfZ;
  });

  return withCandyPhysics(RAPIER.ColliderDesc.convexHull(vertices) ?? RAPIER.ColliderDesc.cuboid(fallbackHalfX, fallbackHalfY, halfZ));
}

function regularPolygonPoints(sides: number, radius: number, offset = 0): Array<[number, number]> {
  return Array.from({ length: sides }, (_, index) => {
    const angle = offset + (Math.PI * 2 * index) / sides;
    return [Math.cos(angle) * radius, Math.sin(angle) * radius];
  });
}

function starPoints(outer: number, inner: number, points: number): Array<[number, number]> {
  return Array.from({ length: points * 2 }, (_, index) => {
    const radius = index % 2 === 0 ? outer : inner;
    const angle = -Math.PI / 2 + (Math.PI * index) / points;
    return [Math.cos(angle) * radius, Math.sin(angle) * radius];
  });
}

function zOnlyQuaternion(): { x: number; y: number; z: number; w: number } {
  const quaternion = new THREE.Quaternion().setFromAxisAngle(
    new THREE.Vector3(0.15, 0.15, 1),
    randomRange(0, Math.PI * 2),
  );
  return { x: quaternion.x, y: quaternion.y, z: quaternion.z, w: quaternion.w };
}

function disposeObject(object: THREE.Object3D): void {
  object.traverse((child) => {
    if (!(child instanceof THREE.Mesh)) return;
    child.geometry.dispose();
    const materials = Array.isArray(child.material) ? child.material : [child.material];
    for (const material of materials) material.dispose();
  });
}

function sizeToWorld(size: number): number {
  return clamp((size / 200) * 3, 0.3, 1.98);
}

function randomRange(min: number, max: number): number {
  return min + Math.random() * (max - min);
}

function clamp(value: number, min: number, max: number): number {
  return Math.min(max, Math.max(min, value));
}
