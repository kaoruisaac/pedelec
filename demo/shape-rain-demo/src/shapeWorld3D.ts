import * as THREE from "three";
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
const WALL_THICKNESS = 0.5;
const DEPTH_LIMIT = 2.4;
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
    this.camera.position.set(0, 1.1, 13);
    this.camera.lookAt(0, -0.35, 0);

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
        const baseX = item.xHint === undefined ? 0 : (item.xHint - 0.5) * (this.viewWidth - radius * 2);
        const x = clamp(baseX + randomRange(-0.8, 0.8), -this.viewWidth * 0.5 + radius, this.viewWidth * 0.5 - radius);
        const y = this.viewHeight * 0.5 + radius * (1.5 + index * 0.55);
        const z = randomRange(-0.45, 0.45);
        const body = this.physics.createRigidBody(
          RAPIER.RigidBodyDesc.dynamic()
            .setTranslation(x, y, z)
            .setRotation(randomQuaternion())
            .setLinvel(randomRange(-0.5, 0.5), randomRange(-0.2, 0.3), randomRange(-0.28, 0.28))
            .setAngvel({ x: randomRange(-1.7, 1.7), y: randomRange(-1.4, 1.4), z: randomRange(-2.4, 2.4) })
            .setCanSleep(true),
        );
        this.physics.createCollider(makeCollider(item.shape, radius), body);

        const mesh = createShapeMesh(item.shape, item.color, radius);
        mesh.position.set(x, y, z);
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
    if (!this.scene) return;
    this.scene.add(new THREE.HemisphereLight(0xffffff, 0xd7e3ff, 2.4));

    const key = new THREE.DirectionalLight(0xffffff, 2.8);
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

    const fill = new THREE.DirectionalLight(0xdceeff, 1.05);
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
    const floorY = -this.viewHeight * 0.5 - WALL_THICKNESS * 0.5 + 0.24;
    const leftX = -this.viewWidth * 0.5 - WALL_THICKNESS * 0.5;
    const rightX = this.viewWidth * 0.5 + WALL_THICKNESS * 0.5;
    this.staticColliders = [
      this.physics.createCollider(
        RAPIER.ColliderDesc.cuboid(this.viewWidth * 0.5 + WALL_THICKNESS, WALL_THICKNESS * 0.5, DEPTH_LIMIT),
      ),
      this.physics.createCollider(
        RAPIER.ColliderDesc.cuboid(WALL_THICKNESS * 0.5, this.viewHeight, DEPTH_LIMIT).setTranslation(leftX, 0, 0),
      ),
      this.physics.createCollider(
        RAPIER.ColliderDesc.cuboid(WALL_THICKNESS * 0.5, this.viewHeight, DEPTH_LIMIT).setTranslation(rightX, 0, 0),
      ),
      this.physics.createCollider(
        RAPIER.ColliderDesc.cuboid(this.viewWidth, this.viewHeight, WALL_THICKNESS * 0.5).setTranslation(0, 0, -DEPTH_LIMIT),
      ),
      this.physics.createCollider(
        RAPIER.ColliderDesc.cuboid(this.viewWidth, this.viewHeight, WALL_THICKNESS * 0.5).setTranslation(0, 0, DEPTH_LIMIT),
      ),
    ];
    this.staticColliders[0].setTranslation({ x: 0, y: floorY, z: 0 });
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
    color: new THREE.Color(color).lerp(new THREE.Color(0xffffff), 0.18),
    roughness: 0.08,
    metalness: 0,
    transmission: 0.62,
    thickness: radius * 1.7,
    attenuationDistance: 2.2,
    attenuationColor: new THREE.Color(color).lerp(new THREE.Color(0xffffff), 0.46),
    clearcoat: 1,
    clearcoatRoughness: 0.12,
    transparent: true,
    opacity: 0.68,
    ior: 1.34,
    envMapIntensity: 1.05,
  });
  const mesh = new THREE.Mesh(geometry, material);
  mesh.castShadow = true;
  mesh.receiveShadow = true;
  group.add(mesh);

  const rim = new THREE.Mesh(
    geometry.clone(),
    new THREE.MeshBasicMaterial({
      color: new THREE.Color(color).lerp(new THREE.Color(0xffffff), 0.7),
      transparent: true,
      opacity: 0.22,
      blending: THREE.AdditiveBlending,
      side: THREE.BackSide,
      depthWrite: false,
    }),
  );
  rim.scale.setScalar(1.045);
  group.add(rim);

  const glint = new THREE.Mesh(
    new THREE.SphereGeometry(Math.max(0.045, radius * 0.12), 12, 8),
    new THREE.MeshBasicMaterial({ color: 0xffffff, transparent: true, opacity: 0.52 }),
  );
  glint.position.set(-radius * 0.24, radius * 0.26, radius * 0.55);
  group.add(glint);
  return group;
}

function makeGeometry(shape: ShapeKind, radius: number): THREE.BufferGeometry {
  if (shape === "circle") return new THREE.SphereGeometry(radius * 0.72, 32, 20);
  if (shape === "square") return new THREE.BoxGeometry(radius * 1.3, radius * 1.3, radius * 0.56, 3, 3, 2);
  if (shape === "rectangle") return new THREE.BoxGeometry(radius * 1.72, radius, radius * 0.54, 3, 2, 2);
  if (shape === "capsule") return makeCapsuleGeometry(radius);
  if (shape === "star") return extrudePoints(starPoints(radius * 0.82, radius * 0.38, 5), radius * 0.46);
  const sides = shape === "triangle" ? 3 : shape === "pentagon" ? 5 : 6;
  return extrudePoints(regularPolygonPoints(sides, radius * 0.78, -Math.PI / 2), radius * 0.46);
}

function makeCapsuleGeometry(radius: number): THREE.BufferGeometry {
  const shape = new THREE.Shape();
  const width = radius * 1.82;
  const height = radius * 0.94;
  const end = height * 0.5;
  shape.absarc(-width * 0.5 + end, 0, end, Math.PI / 2, Math.PI * 1.5, false);
  shape.absarc(width * 0.5 - end, 0, end, Math.PI * 1.5, Math.PI / 2, false);
  shape.closePath();
  return extrudeShape(shape, radius * 0.46);
}

function extrudePoints(points: Array<[number, number]>, depth: number): THREE.BufferGeometry {
  const shape = new THREE.Shape();
  points.forEach(([x, y], index) => {
    if (index === 0) shape.moveTo(x, y);
    else shape.lineTo(x, y);
  });
  shape.closePath();
  return extrudeShape(shape, depth);
}

function extrudeShape(shape: THREE.Shape, depth: number): THREE.BufferGeometry {
  const geometry = new THREE.ExtrudeGeometry(shape, {
    depth,
    bevelEnabled: true,
    bevelSegments: 4,
    bevelSize: depth * 0.18,
    bevelThickness: depth * 0.22,
    curveSegments: 18,
    steps: 1,
  });
  geometry.center();
  return geometry;
}

function makeCollider(shape: ShapeKind, radius: number): RAPIER.ColliderDesc {
  if (shape === "circle") return RAPIER.ColliderDesc.ball(radius * 0.7).setDensity(0.72).setRestitution(0.28).setFriction(0.58);
  const halfX = shape === "rectangle" || shape === "capsule" ? radius * 0.86 : radius * 0.62;
  const halfY = shape === "triangle" ? radius * 0.58 : shape === "star" ? radius * 0.68 : radius * 0.62;
  return RAPIER.ColliderDesc.cuboid(halfX, halfY, radius * 0.28).setDensity(0.72).setRestitution(0.22).setFriction(0.7);
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

function randomQuaternion(): { x: number; y: number; z: number; w: number } {
  const quaternion = new THREE.Quaternion().setFromEuler(
    new THREE.Euler(randomRange(-0.35, 0.35), randomRange(-0.7, 0.7), randomRange(-0.55, 0.55)),
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
  return clamp(size / 58, 0.48, 1.55);
}

function randomRange(min: number, max: number): number {
  return min + Math.random() * (max - min);
}

function clamp(value: number, min: number, max: number): number {
  return Math.min(max, Math.max(min, value));
}
