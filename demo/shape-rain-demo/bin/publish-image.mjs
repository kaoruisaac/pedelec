#!/usr/bin/env node

// node bin/publish-image.mjs
// node bin/publish-image.mjs --tag staging
// node bin/publish-image.mjs --help

import { spawn } from "node:child_process";
import { readFile } from "node:fs/promises";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const IMAGE_NAME = "shape-rain-demo";
const TAG_PATTERN = /^[A-Za-z0-9_][A-Za-z0-9_.-]{0,127}$/;

const binDir = path.dirname(fileURLToPath(import.meta.url));
const demoRoot = path.dirname(binDir);

main().catch((error) => {
  console.error(`Error: ${error.message}`);
  process.exitCode = 1;
});

async function main() {
  const command = parseArgs(process.argv.slice(2));

  if (command.help) {
    printHelp();
    return;
  }

  const env = await readEnvFile();
  const imageRepository = `${getRequiredEnv(env, "DOCKER_REGISTRY_URL")}/${IMAGE_NAME}`;
  const packageJson = await readPackageJson();
  const tags = uniqueTags([packageJson.version, "latest", ...command.tags]);

  for (const tag of tags) {
    validateTag(tag);
  }

  const imageRefs = tags.map((tag) => `${imageRepository}:${tag}`);

  console.log(`Building ${imageRepository}`);
  console.log(`Tags: ${tags.join(", ")}`);
  console.log("");

  await runDocker([
    "build",
    "--file",
    "Dockerfile",
    ...imageRefs.flatMap((imageRef) => ["--tag", imageRef]),
    ".",
  ]);

  console.log("");

  for (const imageRef of imageRefs) {
    await runDocker(["push", imageRef]);
  }

  console.log("");
  console.log("Done.");
}

function parseArgs(args) {
  const command = {
    help: false,
    tags: [],
  };

  for (let index = 0; index < args.length; index += 1) {
    const arg = args[index];

    if (arg === "--help" || arg === "-h") {
      if (args.length > 1) {
        throw new Error("The help flag cannot be combined with other arguments.");
      }

      command.help = true;
      return command;
    }

    if (arg === "--tag" || arg === "-t") {
      const value = args[index + 1];

      if (!value || value.startsWith("-")) {
        throw new Error(`Missing value for ${arg}.`);
      }

      command.tags.push(value);
      index += 1;
      continue;
    }

    if (arg.startsWith("--tag=")) {
      const value = arg.slice("--tag=".length);

      if (!value) {
        throw new Error("Missing value for --tag.");
      }

      command.tags.push(value);
      continue;
    }

    throw new Error(`Unknown argument "${arg}". Use --help for usage.`);
  }

  return command;
}

function printHelp() {
  console.log(`Usage:
  node bin/publish-image.mjs [--tag <tag>...]
  node bin/publish-image.mjs --help

Builds and pushes ${IMAGE_NAME} to DOCKER_REGISTRY_URL from .env.

Default tags:
  package.json version
  latest

Additional tags can be supplied with --tag or -t.`);
}

async function readEnvFile() {
  const envPath = path.join(demoRoot, ".env");
  let content;

  try {
    content = await readFile(envPath, "utf8");
  } catch (error) {
    if (error.code === "ENOENT") {
      throw new Error("Missing .env. Add DOCKER_REGISTRY_URL before publishing.");
    }

    throw new Error(`Failed to read .env: ${error.message}`);
  }

  return parseEnv(content);
}

function parseEnv(content) {
  const env = {};

  for (const line of content.split(/\r?\n/)) {
    const trimmedLine = line.trim();

    if (!trimmedLine || trimmedLine.startsWith("#")) {
      continue;
    }

    const equalsIndex = trimmedLine.indexOf("=");

    if (equalsIndex === -1) {
      continue;
    }

    const key = trimmedLine.slice(0, equalsIndex).trim();
    const value = trimmedLine.slice(equalsIndex + 1).trim();

    if (key) {
      env[key] = unquoteEnvValue(value);
    }
  }

  return env;
}

function unquoteEnvValue(value) {
  if (
    (value.startsWith('"') && value.endsWith('"')) ||
    (value.startsWith("'") && value.endsWith("'"))
  ) {
    return value.slice(1, -1);
  }

  return value;
}

function getRequiredEnv(env, key) {
  const value = env[key];

  if (!value) {
    throw new Error(`Missing ${key} in .env.`);
  }

  return value.replace(/\/+$/, "");
}

async function readPackageJson() {
  const packageJsonPath = path.join(demoRoot, "package.json");
  let data;

  try {
    data = JSON.parse(await readFile(packageJsonPath, "utf8"));
  } catch (error) {
    throw new Error(`Failed to read package.json: ${error.message}`);
  }

  if (!data.version || typeof data.version !== "string") {
    throw new Error("package.json is missing a string version field.");
  }

  return data;
}

function uniqueTags(tags) {
  return [...new Set(tags)];
}

function validateTag(tag) {
  if (!TAG_PATTERN.test(tag)) {
    throw new Error(
      `Invalid Docker tag "${tag}". Use 1-128 characters: letters, numbers, underscore, period, or hyphen; start with a letter, number, or underscore.`,
    );
  }
}

async function runDocker(args) {
  console.log(`docker ${args.join(" ")}`);

  const child = spawn("docker", args, {
    cwd: demoRoot,
    stdio: "inherit",
  });

  await new Promise((resolve, reject) => {
    child.on("error", (error) => {
      if (error.code === "ENOENT") {
        reject(new Error("Docker CLI was not found on PATH."));
        return;
      }

      reject(error);
    });

    child.on("exit", (code) => {
      if (code === 0) {
        resolve();
        return;
      }

      reject(new Error(`docker ${args[0]} failed with exit code ${code}.`));
    });
  });
}
