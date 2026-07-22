import { Provider } from "./types";

export function findFirstAvailableCliProvider(providers: Provider[]): Provider | undefined {
  return providers.find(
    (provider) => provider.code !== "ollama" && provider.scanned && provider.available,
  );
}
