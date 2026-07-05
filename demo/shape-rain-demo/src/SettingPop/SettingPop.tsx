import { createSignal, For, Show } from "solid-js";
import { FiChevronDown, FiX } from "solid-icons/fi";
import { forwardPopUp } from "../services/PopUpProvider";
import "./SettingPop.css";

export type AgentOption = {
  value: string;
  label: string;
};

const AGENT_OPTIONS: AgentOption[] = [
  { value: "", label: "Default" },
  { value: "ollama", label: "Ollama" },
  { value: "claude", label: "Claude" },
  { value: "gemini", label: "Gemini" },
  { value: "codex", label: "Codex" },
  { value: "opencode", label: "OpenCode" },
  { value: "cursor", label: "Cursor" },
];

export type SettingPopProps = {
  agent?: string;
  model?: string;
  onApply?: (settings: { agent: string; model: string }) => void;
};

const SettingPop = forwardPopUp<SettingPopProps>((popup, props) => {
  const [agent, setAgent] = createSignal(props.agent ?? AGENT_OPTIONS[0].value);
  const [model, setModel] = createSignal(props.model ?? "");
  const [agentMenuOpen, setAgentMenuOpen] = createSignal(false);

  const selectedAgentLabel = () => AGENT_OPTIONS.find((option) => option.value === agent())?.label ?? agent();

  function selectAgent(value: string): void {
    setAgent(value);
    setAgentMenuOpen(false);
  }

  function handleApply(): void {
    props.onApply?.({ agent: agent(), model: model().trim() });
    popup.close();
  }

  return (
    <div class="SettingPop" ref={(el) => popup.setDraggableElement(el)}>
      <div class="SettingPop-header">
        <span class="SettingPop-title">Shape Rain Settings</span>
        <button type="button" class="SettingPop-closeBtn" title="Close" onClick={() => popup.close()}>
          <FiX size={16} />
        </button>
      </div>

      <div class="SettingPop-content">
        <div class="SettingPop-field">
          <label class="SettingPop-label">Agent</label>
          <div class="SettingPop-dropdown" classList={{ open: agentMenuOpen() }}>
            <button type="button" class="SettingPop-dropdownTrigger" onClick={() => setAgentMenuOpen((open) => !open)}>
              <span class="SettingPop-dropdownValue">{selectedAgentLabel()}</span>
              <FiChevronDown size={16} />
            </button>
            <Show when={agentMenuOpen()}>
              <div class="SettingPop-dropdownMenu">
                <For each={AGENT_OPTIONS}>
                  {(option) => (
                    <button
                      type="button"
                      class="SettingPop-dropdownOption"
                      classList={{ selected: option.value === agent() }}
                      onClick={() => selectAgent(option.value)}
                    >
                      {option.label}
                    </button>
                  )}
                </For>
              </div>
            </Show>
          </div>
        </div>

        <div class="SettingPop-field">
          <label class="SettingPop-label">Model (optional)</label>
          <div class="SettingPop-inputWrap">
            <input
              class="SettingPop-input"
              type="text"
              value={model()}
              placeholder="e.g. llama3.2"
              onInput={(event) => setModel(event.currentTarget.value)}
            />
          </div>
        </div>
      </div>

      <div class="SettingPop-footer">
        <button type="button" class="SettingPop-cancelBtn" onClick={() => popup.close()}>
          Cancel
        </button>
        <button type="button" class="SettingPop-applyBtn" onClick={handleApply}>
          Apply
        </button>
      </div>
    </div>
  );
});

export default SettingPop;
