import { readdir } from "node:fs/promises";

type WorkspaceEntry = {
  name: string;
  isFile: () => boolean;
  isDirectory: () => boolean;
};

export async function inspectWorkspace(): Promise<{ files: string[]; folders: string[] }> {
  const entries = (await readdir(".", { withFileTypes: true })) as WorkspaceEntry[];

  return {
    files: entries.filter((entry) => entry.isFile()).map((entry) => entry.name),
    folders: entries.filter((entry) => entry.isDirectory()).map((entry) => entry.name),
  };
}
