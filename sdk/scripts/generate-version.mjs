import { readFile, writeFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

const sdkDir = path.dirname(path.dirname(fileURLToPath(import.meta.url)));
const packageJson = JSON.parse(await readFile(path.join(sdkDir, "package.json"), "utf8"));
const generatedPath = path.join(sdkDir, "src", "version.generated.ts");
const content = `// Generated from sdk/package.json. Do not edit manually.\nexport const SDK_VERSION = ${JSON.stringify(packageJson.version)};\n`;

await writeFile(generatedPath, content, "utf8");
