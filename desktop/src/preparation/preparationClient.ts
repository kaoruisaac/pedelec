import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { APP_PREPARATION_EVENT } from "./preparationState";

export interface PreparationClient {
  getState: () => Promise<unknown>;
  retry: () => Promise<unknown>;
  listen: (handler: (snapshot: unknown) => void) => Promise<() => void>;
  acknowledgeFrame: () => Promise<void>;
}

export const preparationClient: PreparationClient = {
  getState: () => invoke("get_app_preparation_state"),
  retry: () => invoke("retry_app_preparation"),
  listen: async (handler) => listen(APP_PREPARATION_EVENT, (event) => {
    handler(event.payload);
  }),
  acknowledgeFrame: () => invoke("acknowledge_preparation_frame"),
};
