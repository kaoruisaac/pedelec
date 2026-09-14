import { pedelecVitePlugin } from "./vite";

const plugin = pedelecVitePlugin();
plugin.name satisfies string;

// The root SDK authoring types intentionally do not expose Node or Vite types.
type RootModule = import("./index").DenoModuleDefinition<"example">;
const definition: RootModule = {
  name: "example",
  description: "Example module.",
  entry: "./example.ts",
  usage: "import { example } from \"example\";",
};
void definition;
