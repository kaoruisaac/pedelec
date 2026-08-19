use crate::{
    error_codes, provider_efforts_args, read_settings_file, validate_effort_tier,
    write_settings_file, CoreRuntime, EffortLevel, EffortsArgs, PedelecError, PedelecSettings,
    ProviderCode, ProviderSettings,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};

const BUNDLED_PRESET_MANIFEST: &str = include_str!("../resources/provider-effort-presets.json");
const MANIFEST_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash, Ord, PartialOrd)]
#[serde(rename_all = "lowercase")]
pub enum WizardProviderCode {
    Codex,
    Claude,
    Cursor,
    Antigravity,
}

impl WizardProviderCode {
    pub const ALL: [Self; 4] = [Self::Codex, Self::Claude, Self::Cursor, Self::Antigravity];

    pub fn all() -> &'static [Self; 4] {
        &Self::ALL
    }

    pub fn as_provider_code(self) -> ProviderCode {
        self.into()
    }

    pub fn from_provider_code(provider: &ProviderCode) -> Option<Self> {
        match provider {
            ProviderCode::Codex => Some(Self::Codex),
            ProviderCode::Claude => Some(Self::Claude),
            ProviderCode::Cursor => Some(Self::Cursor),
            ProviderCode::Antigravity => Some(Self::Antigravity),
            ProviderCode::OpenCode | ProviderCode::Ollama => None,
        }
    }

    pub fn from_provider(provider: &ProviderCode) -> Option<Self> {
        Self::from_provider_code(provider)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Cursor => "cursor",
            Self::Antigravity => "antigravity",
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::Claude => "Claude Code",
            Self::Cursor => "Cursor",
            Self::Antigravity => "Antigravity",
        }
    }
}

impl From<WizardProviderCode> for ProviderCode {
    fn from(provider: WizardProviderCode) -> Self {
        match provider {
            WizardProviderCode::Codex => ProviderCode::Codex,
            WizardProviderCode::Claude => ProviderCode::Claude,
            WizardProviderCode::Cursor => ProviderCode::Cursor,
            WizardProviderCode::Antigravity => ProviderCode::Antigravity,
        }
    }
}

impl TryFrom<ProviderCode> for WizardProviderCode {
    type Error = ();

    fn try_from(provider: ProviderCode) -> Result<Self, Self::Error> {
        Self::from_provider_code(&provider).ok_or(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct EffortWizardMetadata {
    #[serde(default)]
    pub providers: BTreeMap<WizardProviderCode, EffortWizardProviderMetadata>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct EffortWizardProviderMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied_preset_revision: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct EffortWizardManifest {
    pub schema_version: u32,
    pub providers: BTreeMap<WizardProviderCode, EffortWizardPreset>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct EffortWizardPreset {
    pub revision: u32,
    pub profiles: EffortsArgs,
    pub entry_probe: String,
    pub probes: Vec<EffortWizardProbeDefinition>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct EffortWizardProbeDefinition {
    pub id: String,
    pub args: Vec<String>,
    pub on_supported: EffortWizardTransition,
    pub on_not_entitled: EffortWizardTransition,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct EffortWizardTransition {
    #[serde(default)]
    pub confirms: Vec<EffortLevel>,
    #[serde(default)]
    pub next_probe: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct EffortWizardProbePlan {
    pub provider: WizardProviderCode,
    pub revision: u32,
    pub profiles: EffortsArgs,
    pub entry_probe: String,
    pub probes: Vec<EffortWizardProbeDefinition>,
}

impl EffortWizardProbePlan {
    pub fn probe_definition(&self, probe_id: &str) -> Option<&EffortWizardProbeDefinition> {
        self.probes.iter().find(|probe| probe.id == probe_id)
    }

    pub fn transition_after_supported(
        &self,
        probe_id: &str,
    ) -> Result<&EffortWizardTransition, PedelecError> {
        self.probe_definition(probe_id)
            .map(|probe| &probe.on_supported)
            .ok_or_else(|| invalid_plan_error(self.provider, "probe does not exist"))
    }

    pub fn transition_after_not_entitled(
        &self,
        probe_id: &str,
    ) -> Result<&EffortWizardTransition, PedelecError> {
        self.probe_definition(probe_id)
            .map(|probe| &probe.on_not_entitled)
            .ok_or_else(|| invalid_plan_error(self.provider, "probe does not exist"))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct EffortWizardConfirmedProfiles {
    pub low: Option<Vec<String>>,
    pub default: Option<Vec<String>>,
    pub high: Option<Vec<String>>,
}

impl EffortWizardConfirmedProfiles {
    pub fn has_any(&self) -> bool {
        self.low.is_some() || self.default.is_some() || self.high.is_some()
    }

    fn get(&self, level: EffortLevel) -> Option<&Vec<String>> {
        match level {
            EffortLevel::Low => self.low.as_ref(),
            EffortLevel::Default => self.default.as_ref(),
            EffortLevel::High => self.high.as_ref(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct EffortWizardProviderRecommendation {
    pub provider: WizardProviderCode,
    pub preset_revision: u32,
    pub confirmed: EffortWizardConfirmedProfiles,
    pub deterministic_complete: bool,
}

impl EffortWizardProviderRecommendation {
    pub fn has_confirmed_tier(&self) -> bool {
        self.confirmed.has_any()
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum EffortWizardTierDecision {
    KeepCurrent,
    Update,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub struct EffortWizardTierDecisions {
    pub low: EffortWizardTierDecision,
    pub default: EffortWizardTierDecision,
    pub high: EffortWizardTierDecision,
}

impl Default for EffortWizardTierDecisions {
    fn default() -> Self {
        Self {
            low: EffortWizardTierDecision::KeepCurrent,
            default: EffortWizardTierDecision::KeepCurrent,
            high: EffortWizardTierDecision::KeepCurrent,
        }
    }
}

impl EffortWizardTierDecisions {
    fn get(self, level: EffortLevel) -> EffortWizardTierDecision {
        match level {
            EffortLevel::Low => self.low,
            EffortLevel::Default => self.default,
            EffortLevel::High => self.high,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct EffortWizardProviderApplyPatch {
    pub provider: WizardProviderCode,
    pub preset_revision: u32,
    pub expected_current_efforts: EffortsArgs,
    pub confirmed_recommendation: EffortWizardProviderRecommendation,
    pub tier_decisions: EffortWizardTierDecisions,
    pub can_advance_revision: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct EffortWizardApplyPatch {
    pub providers: Vec<EffortWizardProviderApplyPatch>,
}

pub type ProviderProbePlan = EffortWizardProbePlan;
pub type ProviderRecommendation = EffortWizardProviderRecommendation;
pub type ProviderApplyPatch = EffortWizardProviderApplyPatch;
pub type TierDecision = EffortWizardTierDecision;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct EffortWizardProviderBootstrap {
    pub provider: WizardProviderCode,
    pub available: bool,
    pub version: Option<String>,
    pub current_preset_revision: u32,
    pub applied_preset_revision: Option<u32>,
    pub has_any_effort_setting: bool,
    pub preset_update_available: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum EffortWizardHomeReminder {
    InitialSetup,
    PresetUpdate { providers: Vec<WizardProviderCode> },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct EffortWizardBootstrap {
    pub providers: Vec<EffortWizardProviderBootstrap>,
    pub home_reminder: Option<EffortWizardHomeReminder>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum EffortWizardEntryPoint {
    InitialSetup,
    Settings,
    PresetUpdate,
}

pub fn bundled_effort_wizard_manifest() -> Result<EffortWizardManifest, PedelecError> {
    parse_effort_wizard_manifest(BUNDLED_PRESET_MANIFEST)
}

pub fn parse_effort_wizard_manifest(json: &str) -> Result<EffortWizardManifest, PedelecError> {
    let raw = serde_json::from_str::<Value>(json).map_err(|error| {
        invalid_manifest_error(
            "manifest is not valid JSON",
            serde_json::json!({
                "error": error.to_string(),
            }),
        )
    })?;

    let providers = raw
        .get("providers")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid_manifest_error("providers must be an object", Value::Null))?;

    for key in providers.keys() {
        if matches!(key.as_str(), "opencode" | "ollama") {
            return Err(invalid_manifest_error(
                "OpenCode and Ollama are not supported by the effort wizard",
                serde_json::json!({ "provider": key }),
            ));
        }
        if !WizardProviderCode::all()
            .iter()
            .any(|provider| provider.as_str() == key)
        {
            return Err(invalid_manifest_error(
                "manifest contains an unsupported provider",
                serde_json::json!({ "provider": key }),
            ));
        }
    }

    if providers.len() != WizardProviderCode::all().len() {
        return Err(invalid_manifest_error(
            "manifest must contain exactly the four wizard-supported providers",
            serde_json::json!({ "providers": providers.keys().collect::<Vec<_>>() }),
        ));
    }

    validate_manifest_json_shape(providers)?;

    let manifest = serde_json::from_value::<EffortWizardManifest>(raw).map_err(|error| {
        invalid_manifest_error(
            "manifest schema is invalid",
            serde_json::json!({
                "error": error.to_string(),
            }),
        )
    })?;

    validate_effort_wizard_manifest(&manifest)?;
    Ok(manifest)
}

fn validate_manifest_json_shape(
    providers: &serde_json::Map<String, Value>,
) -> Result<(), PedelecError> {
    for (provider, value) in providers {
        let preset = value.as_object().ok_or_else(|| {
            invalid_manifest_error(
                "provider preset must be an object",
                serde_json::json!({ "provider": provider }),
            )
        })?;
        let profiles = preset
            .get("profiles")
            .and_then(Value::as_object)
            .ok_or_else(|| {
                invalid_manifest_error(
                    "provider preset profiles must be an object",
                    serde_json::json!({ "provider": provider }),
                )
            })?;
        for tier in ["low", "default", "high"] {
            if !profiles.get(tier).is_some_and(Value::is_array) {
                return Err(invalid_manifest_error(
                    "provider preset must define low, default, and high profile arrays",
                    serde_json::json!({ "provider": provider, "profile": tier }),
                ));
            }
        }

        let probes = preset
            .get("probes")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                invalid_manifest_error(
                    "provider preset probes must be an array",
                    serde_json::json!({ "provider": provider }),
                )
            })?;
        for probe in probes {
            let probe = probe.as_object().ok_or_else(|| {
                invalid_manifest_error(
                    "probe definition must be an object",
                    serde_json::json!({ "provider": provider }),
                )
            })?;
            for transition in ["onSupported", "onNotEntitled"] {
                let transition_value = probe.get(transition).and_then(Value::as_object);
                if !transition_value
                    .is_some_and(|value| value.get("confirms").is_some_and(Value::is_array))
                {
                    return Err(invalid_manifest_error(
                        "probe transition must explicitly define confirms",
                        serde_json::json!({
                            "provider": provider,
                            "transition": transition,
                        }),
                    ));
                }
            }
        }
    }
    Ok(())
}

pub fn validate_effort_wizard_manifest(
    manifest: &EffortWizardManifest,
) -> Result<(), PedelecError> {
    if manifest.schema_version != MANIFEST_SCHEMA_VERSION {
        return Err(invalid_manifest_error(
            "unsupported manifest schema version",
            serde_json::json!({ "schemaVersion": manifest.schema_version }),
        ));
    }

    if manifest.providers.len() != WizardProviderCode::all().len()
        || WizardProviderCode::all()
            .iter()
            .any(|provider| !manifest.providers.contains_key(provider))
    {
        return Err(invalid_manifest_error(
            "manifest must contain exactly the four wizard-supported providers",
            Value::Null,
        ));
    }

    for provider in WizardProviderCode::all() {
        let preset = manifest.providers.get(provider).ok_or_else(|| {
            invalid_manifest_error(
                "manifest provider is missing",
                serde_json::json!({
                    "provider": provider.as_str(),
                }),
            )
        })?;
        let provider_code = provider.as_provider_code();

        if preset.revision == 0 {
            return Err(invalid_manifest_error(
                "provider revision must be greater than zero",
                serde_json::json!({ "provider": provider.as_str() }),
            ));
        }

        validate_effort_tier(&provider_code, EffortLevel::Low, &preset.profiles.low)
            .map_err(|error| preset_validation_error(*provider, "low", error))?;
        validate_effort_tier(
            &provider_code,
            EffortLevel::Default,
            &preset.profiles.default,
        )
        .map_err(|error| preset_validation_error(*provider, "default", error))?;
        validate_effort_tier(&provider_code, EffortLevel::High, &preset.profiles.high)
            .map_err(|error| preset_validation_error(*provider, "high", error))?;

        if preset.probes.is_empty() || preset.probes.len() > 2 {
            return Err(invalid_manifest_error(
                "each provider must define between one and two probes",
                serde_json::json!({
                    "provider": provider.as_str(),
                    "probeCount": preset.probes.len(),
                }),
            ));
        }

        let mut ids = HashSet::new();
        for probe in &preset.probes {
            if probe.id.trim().is_empty() || !ids.insert(probe.id.clone()) {
                return Err(invalid_manifest_error(
                    "probe ids must be non-empty and unique",
                    serde_json::json!({ "provider": provider.as_str(), "probe": probe.id }),
                ));
            }
            validate_effort_tier(&provider_code, EffortLevel::Default, &probe.args)
                .map_err(|error| preset_validation_error(*provider, &probe.id, error))?;
            validate_transition_targets(*provider, preset, &probe.on_supported)?;
            validate_transition_targets(*provider, preset, &probe.on_not_entitled)?;
            validate_confirmations(*provider, &probe.on_supported.confirms)?;
            validate_confirmations(*provider, &probe.on_not_entitled.confirms)?;
        }

        if !ids.contains(&preset.entry_probe) {
            return Err(invalid_manifest_error(
                "entryProbe must point to an existing probe",
                serde_json::json!({
                    "provider": provider.as_str(),
                    "entryProbe": preset.entry_probe,
                }),
            ));
        }

        let reachable = reachable_probe_ids(*provider, preset)?;
        if reachable.len() > 2 {
            return Err(invalid_manifest_error(
                "a provider probe plan cannot reach more than two probes",
                serde_json::json!({ "provider": provider.as_str(), "reachableCount": reachable.len() }),
            ));
        }
    }

    Ok(())
}

pub fn build_effort_wizard_probe_plan(
    provider: WizardProviderCode,
) -> Result<EffortWizardProbePlan, PedelecError> {
    let manifest = bundled_effort_wizard_manifest()?;
    let preset = manifest
        .providers
        .get(&provider)
        .ok_or_else(|| invalid_plan_error(provider, "provider has no bundled preset"))?;
    Ok(EffortWizardProbePlan {
        provider,
        revision: preset.revision,
        profiles: preset.profiles.clone(),
        entry_probe: preset.entry_probe.clone(),
        probes: preset.probes.clone(),
    })
}

impl CoreRuntime {
    pub fn get_effort_wizard_bootstrap(&self) -> Result<EffortWizardBootstrap, PedelecError> {
        let manifest = bundled_effort_wizard_manifest()?;
        let settings = self.get_settings()?;
        let available = self.list_providers();
        let providers = WizardProviderCode::all()
            .iter()
            .map(|provider| {
                let provider_info = available
                    .iter()
                    .find(|info| info.code == provider.as_provider_code());
                let current_preset_revision = manifest
                    .providers
                    .get(provider)
                    .map(|preset| preset.revision)
                    .unwrap_or_default();
                let applied_preset_revision = settings
                    .wizard_metadata
                    .providers
                    .get(provider)
                    .and_then(|metadata| metadata.applied_preset_revision);
                let has_any_effort_setting = provider_has_any_effort_setting(&settings, *provider);
                let available_now = provider_info.is_some_and(|info| info.available);
                EffortWizardProviderBootstrap {
                    provider: *provider,
                    available: available_now,
                    version: provider_info.and_then(|info| info.version.clone()),
                    current_preset_revision,
                    applied_preset_revision,
                    has_any_effort_setting,
                    preset_update_available: available_now
                        && applied_preset_revision
                            .is_some_and(|applied| current_preset_revision > applied),
                }
            })
            .collect::<Vec<_>>();

        let home_reminder = effort_wizard_home_reminder(&providers);
        Ok(EffortWizardBootstrap {
            providers,
            home_reminder,
        })
    }

    pub fn effort_wizard_bootstrap(&self) -> Result<EffortWizardBootstrap, PedelecError> {
        self.get_effort_wizard_bootstrap()
    }

    pub fn default_effort_wizard_selection(
        &self,
        entry_point: EffortWizardEntryPoint,
    ) -> Result<Vec<WizardProviderCode>, PedelecError> {
        let bootstrap = self.get_effort_wizard_bootstrap()?;
        Ok(bootstrap
            .providers
            .into_iter()
            .filter(|provider| provider.available)
            .filter(|provider| {
                !matches!(entry_point, EffortWizardEntryPoint::PresetUpdate)
                    || provider.preset_update_available
            })
            .map(|provider| provider.provider)
            .collect())
    }

    pub fn validate_effort_wizard_selection(
        &self,
        selected: &[WizardProviderCode],
    ) -> Result<(), PedelecError> {
        validate_effort_wizard_selection(selected)?;
        let available = self.list_providers();
        if let Some(provider) = selected.iter().find(|provider| {
            !available
                .iter()
                .any(|info| info.available && info.code == provider.as_provider_code())
        }) {
            return Err(apply_invalid_error(
                "an unavailable provider cannot be selected for probing",
                serde_json::json!({ "provider": provider.as_str() }),
            ));
        }
        Ok(())
    }

    pub fn apply_effort_wizard_patch(
        &self,
        patch: EffortWizardApplyPatch,
    ) -> Result<PedelecSettings, PedelecError> {
        let manifest = bundled_effort_wizard_manifest()?;
        let settings_path = self.resolved_settings_file_path()?;
        let mut settings = read_settings_file(&settings_path)?;
        let mut seen = HashSet::new();

        for provider_patch in &patch.providers {
            validate_apply_patch(provider_patch, &manifest, &settings, &mut seen)?;
        }

        if patch.providers.is_empty() {
            return Ok(settings);
        }

        for provider_patch in patch.providers {
            let provider = provider_patch.provider;
            let target = provider_efforts_args_mut(&mut settings.provider_settings, provider);
            apply_tier_decisions(
                target,
                &provider_patch.confirmed_recommendation.confirmed,
                provider_patch.tier_decisions,
            )?;

            if provider_patch.can_advance_revision {
                settings.wizard_metadata.providers.insert(
                    provider,
                    EffortWizardProviderMetadata {
                        applied_preset_revision: Some(provider_patch.preset_revision),
                    },
                );
            }
        }

        write_settings_file(&settings_path, &settings)?;
        Ok(settings)
    }

    pub fn apply_effort_wizard(
        &self,
        patch: EffortWizardApplyPatch,
    ) -> Result<PedelecSettings, PedelecError> {
        self.apply_effort_wizard_patch(patch)
    }
}

pub fn effort_wizard_home_reminder(
    providers: &[EffortWizardProviderBootstrap],
) -> Option<EffortWizardHomeReminder> {
    let any_available = providers.iter().any(|provider| provider.available);
    let all_efforts_empty = providers
        .iter()
        .all(|provider| !provider.has_any_effort_setting);
    if any_available && all_efforts_empty {
        return Some(EffortWizardHomeReminder::InitialSetup);
    }

    let outdated = providers
        .iter()
        .filter(|provider| provider.preset_update_available)
        .map(|provider| provider.provider)
        .collect::<Vec<_>>();
    (!outdated.is_empty()).then_some(EffortWizardHomeReminder::PresetUpdate {
        providers: outdated,
    })
}

pub fn validate_effort_wizard_selection(
    selected: &[WizardProviderCode],
) -> Result<(), PedelecError> {
    let mut seen = HashSet::new();
    for provider in selected {
        if !seen.insert(*provider) {
            return Err(apply_invalid_error(
                "a provider may be selected only once",
                serde_json::json!({ "provider": provider.as_str() }),
            ));
        }
    }
    if selected.is_empty() {
        return Err(apply_invalid_error(
            "at least one provider must be selected for probing",
            Value::Null,
        ));
    }
    Ok(())
}

fn provider_has_any_effort_setting(
    settings: &PedelecSettings,
    provider: WizardProviderCode,
) -> bool {
    let efforts = provider_efforts_args(&settings.provider_settings, &provider.as_provider_code());
    !efforts.low.is_empty() || !efforts.default.is_empty() || !efforts.high.is_empty()
}

fn validate_apply_patch(
    patch: &EffortWizardProviderApplyPatch,
    manifest: &EffortWizardManifest,
    settings: &PedelecSettings,
    seen: &mut HashSet<WizardProviderCode>,
) -> Result<(), PedelecError> {
    if !seen.insert(patch.provider) {
        return Err(apply_invalid_error(
            "a provider may appear only once in an apply patch",
            serde_json::json!({ "provider": patch.provider.as_str() }),
        ));
    }

    let preset = manifest.providers.get(&patch.provider).ok_or_else(|| {
        apply_invalid_error(
            "provider has no bundled preset",
            serde_json::json!({ "provider": patch.provider.as_str() }),
        )
    })?;
    if patch.preset_revision != preset.revision
        || patch.confirmed_recommendation.preset_revision != preset.revision
    {
        return Err(apply_invalid_error(
            "apply patch revision does not match the bundled preset",
            serde_json::json!({
                "provider": patch.provider.as_str(),
                "expectedRevision": preset.revision,
                "receivedRevision": patch.preset_revision,
            }),
        ));
    }
    if patch.confirmed_recommendation.provider != patch.provider {
        return Err(apply_invalid_error(
            "recommendation provider does not match apply provider",
            serde_json::json!({ "provider": patch.provider.as_str() }),
        ));
    }
    if !patch.confirmed_recommendation.deterministic_complete {
        return Err(apply_invalid_error(
            "only deterministic recommendations can be applied",
            serde_json::json!({ "provider": patch.provider.as_str() }),
        ));
    }
    if !patch.confirmed_recommendation.has_confirmed_tier() {
        return Err(apply_invalid_error(
            "a recommendation must confirm at least one tier",
            serde_json::json!({ "provider": patch.provider.as_str() }),
        ));
    }

    let expected_current = provider_efforts_args(
        &settings.provider_settings,
        &patch.provider.as_provider_code(),
    );
    if expected_current != &patch.expected_current_efforts {
        return Err(PedelecError::with_details(
            error_codes::EFFORT_WIZARD_SETTINGS_CHANGED,
            "provider effort settings changed while the wizard preview was open",
            serde_json::json!({ "provider": patch.provider.as_str() }),
        ));
    }

    for level in [EffortLevel::Low, EffortLevel::Default, EffortLevel::High] {
        if let Some(confirmed) = patch.confirmed_recommendation.confirmed.get(level) {
            let expected = match level {
                EffortLevel::Low => &preset.profiles.low,
                EffortLevel::Default => &preset.profiles.default,
                EffortLevel::High => &preset.profiles.high,
            };
            if confirmed != expected {
                return Err(apply_invalid_error(
                    "recommendation args must come from the bundled preset",
                    serde_json::json!({
                        "provider": patch.provider.as_str(),
                        "effortLevel": effort_level_as_str(level),
                    }),
                ));
            }
        } else if patch.tier_decisions.get(level) == EffortWizardTierDecision::Update {
            return Err(apply_invalid_error(
                "an unconfirmed tier cannot be updated",
                serde_json::json!({
                    "provider": patch.provider.as_str(),
                    "effortLevel": effort_level_as_str(level),
                }),
            ));
        }
    }

    Ok(())
}

fn apply_tier_decisions(
    current: &mut EffortsArgs,
    confirmed: &EffortWizardConfirmedProfiles,
    decisions: EffortWizardTierDecisions,
) -> Result<(), PedelecError> {
    for level in [EffortLevel::Low, EffortLevel::Default, EffortLevel::High] {
        if decisions.get(level) != EffortWizardTierDecision::Update {
            continue;
        }
        let Some(recommendation) = confirmed.get(level) else {
            return Err(apply_invalid_error(
                "an unconfirmed tier cannot be updated",
                serde_json::json!({ "effortLevel": effort_level_as_str(level) }),
            ));
        };
        match level {
            EffortLevel::Low => current.low = recommendation.clone(),
            EffortLevel::Default => current.default = recommendation.clone(),
            EffortLevel::High => current.high = recommendation.clone(),
        }
    }
    Ok(())
}

fn provider_efforts_args_mut(
    settings: &mut ProviderSettings,
    provider: WizardProviderCode,
) -> &mut EffortsArgs {
    match provider.as_provider_code() {
        ProviderCode::Codex => &mut settings.codex.efforts_args,
        ProviderCode::Claude => &mut settings.claude.efforts_args,
        ProviderCode::Cursor => &mut settings.cursor.efforts_args,
        ProviderCode::Antigravity => &mut settings.antigravity.efforts_args,
        ProviderCode::OpenCode | ProviderCode::Ollama => {
            unreachable!("WizardProviderCode conversion must not produce unsupported providers")
        }
    }
}

fn validate_transition_targets(
    provider: WizardProviderCode,
    preset: &EffortWizardPreset,
    transition: &EffortWizardTransition,
) -> Result<(), PedelecError> {
    if transition.next_probe.as_ref().is_some_and(|next| {
        !preset
            .probes
            .iter()
            .any(|probe| probe.id.as_str() == next.as_str())
    }) {
        return Err(invalid_manifest_error(
            "nextProbe must point to an existing probe",
            serde_json::json!({ "provider": provider.as_str(), "nextProbe": transition.next_probe }),
        ));
    }
    Ok(())
}

fn validate_confirmations(
    provider: WizardProviderCode,
    confirmations: &[EffortLevel],
) -> Result<(), PedelecError> {
    let mut seen = HashSet::new();
    for level in confirmations {
        if !seen.insert(*level) {
            return Err(invalid_manifest_error(
                "a transition cannot confirm the same tier twice",
                serde_json::json!({
                    "provider": provider.as_str(),
                    "effortLevel": effort_level_as_str(*level),
                }),
            ));
        }
    }
    Ok(())
}

fn reachable_probe_ids(
    provider: WizardProviderCode,
    preset: &EffortWizardPreset,
) -> Result<HashSet<String>, PedelecError> {
    let definitions = preset
        .probes
        .iter()
        .map(|probe| (probe.id.as_str(), probe))
        .collect::<HashMap<_, _>>();
    let mut states = HashMap::<String, VisitState>::new();
    let mut reachable = HashSet::new();
    visit_probe(
        provider,
        &preset.entry_probe,
        &definitions,
        &mut states,
        &mut reachable,
    )?;
    Ok(reachable)
}

fn visit_probe(
    provider: WizardProviderCode,
    probe_id: &str,
    definitions: &HashMap<&str, &EffortWizardProbeDefinition>,
    states: &mut HashMap<String, VisitState>,
    reachable: &mut HashSet<String>,
) -> Result<(), PedelecError> {
    match states.get(probe_id) {
        Some(VisitState::Visiting) => {
            return Err(invalid_manifest_error(
                "probe transitions must not contain a cycle",
                serde_json::json!({ "provider": provider.as_str(), "probe": probe_id }),
            ));
        }
        Some(VisitState::Visited) => return Ok(()),
        None => {}
    }
    let definition = definitions.get(probe_id).ok_or_else(|| {
        invalid_manifest_error(
            "probe transition points to an unknown probe",
            serde_json::json!({ "provider": provider.as_str(), "probe": probe_id }),
        )
    })?;
    states.insert(probe_id.to_string(), VisitState::Visiting);
    reachable.insert(probe_id.to_string());
    for next_probe in [
        definition.on_supported.next_probe.as_deref(),
        definition.on_not_entitled.next_probe.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        visit_probe(provider, next_probe, definitions, states, reachable)?;
    }
    states.insert(probe_id.to_string(), VisitState::Visited);
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VisitState {
    Visiting,
    Visited,
}

fn invalid_manifest_error(message: &str, details: Value) -> PedelecError {
    PedelecError::with_details(error_codes::EFFORT_WIZARD_PRESET_INVALID, message, details)
}

fn preset_validation_error(
    provider: WizardProviderCode,
    field: &str,
    error: PedelecError,
) -> PedelecError {
    invalid_manifest_error(
        "provider effort preset contains invalid effort args",
        serde_json::json!({
            "provider": provider.as_str(),
            "field": field,
            "error": error,
        }),
    )
}

fn invalid_plan_error(provider: WizardProviderCode, message: &str) -> PedelecError {
    PedelecError::with_details(
        error_codes::EFFORT_WIZARD_PRESET_INVALID,
        message,
        serde_json::json!({ "provider": provider.as_str() }),
    )
}

fn apply_invalid_error(message: &str, details: Value) -> PedelecError {
    PedelecError::with_details(error_codes::EFFORT_WIZARD_APPLY_INVALID, message, details)
}

fn effort_level_as_str(level: EffortLevel) -> &'static str {
    match level {
        EffortLevel::Default => "default",
        EffortLevel::Low => "low",
        EffortLevel::High => "high",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{OllamaProviderSettings, ProviderSettingsInput, SdkSettings, UpdateSettingsInput};
    use serde_json::json;
    use std::ffi::OsString;
    use tempfile::tempdir;

    fn manifest_value() -> Value {
        serde_json::from_str(BUNDLED_PRESET_MANIFEST).unwrap()
    }

    fn runtime_with_settings(settings: PedelecSettings) -> (tempfile::TempDir, CoreRuntime) {
        let temp = tempdir().unwrap();
        let path = temp.path().join("settings.json");
        write_settings_file(&path, &settings).unwrap();
        (
            temp,
            CoreRuntime {
                settings_file_path: Some(path),
                ..CoreRuntime::default()
            },
        )
    }

    fn patch_for(
        provider: WizardProviderCode,
        current: EffortsArgs,
        confirmed: EffortWizardConfirmedProfiles,
        decisions: EffortWizardTierDecisions,
        can_advance_revision: bool,
    ) -> EffortWizardApplyPatch {
        let revision = bundled_effort_wizard_manifest()
            .unwrap()
            .providers
            .get(&provider)
            .unwrap()
            .revision;
        EffortWizardApplyPatch {
            providers: vec![EffortWizardProviderApplyPatch {
                provider,
                preset_revision: revision,
                expected_current_efforts: current,
                confirmed_recommendation: EffortWizardProviderRecommendation {
                    provider,
                    preset_revision: revision,
                    confirmed,
                    deterministic_complete: true,
                },
                tier_decisions: decisions,
                can_advance_revision,
            }],
        }
    }

    #[test]
    fn bundled_manifest_loads_with_exact_initial_matrix() {
        let manifest = bundled_effort_wizard_manifest().unwrap();
        assert_eq!(manifest.schema_version, 1);
        assert_eq!(manifest.providers.len(), 4);
        for provider in WizardProviderCode::all() {
            assert_eq!(manifest.providers[provider].revision, 1);
        }
        assert_eq!(
            manifest.providers[&WizardProviderCode::Codex].profiles.low,
            vec![
                "-m",
                "gpt-5.6-luna",
                "-c",
                "model_reasoning_effort=\"high\""
            ]
        );
        assert_eq!(
            manifest.providers[&WizardProviderCode::Claude]
                .profiles
                .default,
            vec!["--model", "claude-opus-4-8", "--effort", "medium"]
        );
        assert_eq!(
            manifest.providers[&WizardProviderCode::Cursor]
                .profiles
                .high,
            vec!["--model", "claude-opus-5-high"]
        );
        assert_eq!(
            manifest.providers[&WizardProviderCode::Antigravity]
                .profiles
                .default,
            vec!["--model", "gemini-3.7-flash-high", "--effort", "high"]
        );
    }

    #[test]
    fn manifest_validation_rejects_invalid_schema_provider_graph_and_args() {
        let mut value = manifest_value();
        value["schemaVersion"] = json!(2);
        assert_eq!(
            parse_effort_wizard_manifest(&value.to_string())
                .unwrap_err()
                .code,
            error_codes::EFFORT_WIZARD_PRESET_INVALID
        );

        let mut value = manifest_value();
        value["providers"]["opencode"] = value["providers"]["codex"].clone();
        assert!(parse_effort_wizard_manifest(&value.to_string()).is_err());

        let mut value = manifest_value();
        value["providers"]["codex"]["probes"] = json!([
            value["providers"]["codex"]["probes"][0].clone(),
            value["providers"]["codex"]["probes"][1].clone(),
            value["providers"]["codex"]["probes"][1].clone()
        ]);
        assert!(parse_effort_wizard_manifest(&value.to_string()).is_err());

        let mut value = manifest_value();
        value["providers"]["codex"]["entryProbe"] = json!("missing");
        assert!(parse_effort_wizard_manifest(&value.to_string()).is_err());

        let mut value = manifest_value();
        value["providers"]["codex"]["probes"][0]["onSupported"]["nextProbe"] = json!("missing");
        assert!(parse_effort_wizard_manifest(&value.to_string()).is_err());

        let mut value = manifest_value();
        value["providers"]["codex"]["probes"][1]["onSupported"]["nextProbe"] = json!("premium");
        assert!(parse_effort_wizard_manifest(&value.to_string()).is_err());

        let mut value = manifest_value();
        value["providers"]["codex"]["profiles"]["low"] = json!(["--invalid", "x"]);
        assert!(parse_effort_wizard_manifest(&value.to_string()).is_err());
    }

    #[test]
    fn wizard_provider_conversion_excludes_opencode_and_ollama() {
        assert_eq!(
            WizardProviderCode::from_provider_code(&ProviderCode::Codex),
            Some(WizardProviderCode::Codex)
        );
        assert_eq!(
            WizardProviderCode::from_provider_code(&ProviderCode::OpenCode),
            None
        );
        assert_eq!(
            WizardProviderCode::from_provider_code(&ProviderCode::Ollama),
            None
        );
        assert_eq!(
            WizardProviderCode::all(),
            &[
                WizardProviderCode::Codex,
                WizardProviderCode::Claude,
                WizardProviderCode::Cursor,
                WizardProviderCode::Antigravity
            ]
        );
    }

    #[test]
    fn probe_selection_rejects_empty_and_duplicate_inputs() {
        assert!(validate_effort_wizard_selection(&[]).is_err());
        assert!(validate_effort_wizard_selection(&[
            WizardProviderCode::Codex,
            WizardProviderCode::Codex,
        ])
        .is_err());
        assert!(validate_effort_wizard_selection(&[WizardProviderCode::Codex]).is_ok());
    }

    #[test]
    fn plan_exposes_manifest_owned_transitions_without_inference() {
        let plan = build_effort_wizard_probe_plan(WizardProviderCode::Claude).unwrap();
        assert_eq!(plan.entry_probe, "premium");
        assert_eq!(
            plan.transition_after_supported("premium").unwrap().confirms,
            vec![EffortLevel::High]
        );
        assert_eq!(
            plan.transition_after_supported("baseline")
                .unwrap()
                .confirms,
            vec![EffortLevel::Low, EffortLevel::Default]
        );
        assert_eq!(
            plan.transition_after_not_entitled("premium")
                .unwrap()
                .next_probe,
            Some("baseline".into())
        );
    }

    #[test]
    fn legacy_settings_and_sdk_contract_do_not_expose_wizard_metadata() {
        let settings = serde_json::from_value::<PedelecSettings>(json!({
            "defaultProvider": "codex",
            "providerSettings": {}
        }))
        .unwrap();
        assert!(settings.wizard_metadata.providers.is_empty());
        assert!(serde_json::to_value(SdkSettings::from(settings))
            .unwrap()
            .get("wizardMetadata")
            .is_none());
        assert!(serde_json::to_value(PedelecSettings::default())
            .unwrap()
            .get("wizardMetadata")
            .is_some());
    }

    #[test]
    fn empty_persisted_wizard_supported_tiers_stay_empty_at_runtime() {
        let settings = PedelecSettings::default();
        for provider in [
            ProviderCode::Codex,
            ProviderCode::Claude,
            ProviderCode::Cursor,
            ProviderCode::Antigravity,
        ] {
            for level in [EffortLevel::Low, EffortLevel::Default, EffortLevel::High] {
                assert_eq!(
                    crate::resolve_thread_effort_args(&settings, &provider, level).unwrap(),
                    Vec::<String>::new()
                );
            }
        }
    }

    #[test]
    fn generic_settings_update_preserves_wizard_metadata() {
        let mut settings = PedelecSettings::default();
        settings.wizard_metadata.providers.insert(
            WizardProviderCode::Codex,
            EffortWizardProviderMetadata {
                applied_preset_revision: Some(1),
            },
        );
        let (temp, mut runtime) = runtime_with_settings(settings);
        runtime.provider_path_value_override = Some(OsString::new());
        let mut provider_settings = ProviderSettingsInput::default();
        provider_settings.ollama.api_key = Some("ollama".into());
        provider_settings.ollama.efforts_args.default = vec!["--model".into(), "qwen3:8b".into()];
        runtime
            .update_settings(UpdateSettingsInput {
                default_provider: ProviderCode::Ollama,
                provider_settings,
            })
            .unwrap();
        let saved = read_settings_file(&temp.path().join("settings.json")).unwrap();
        assert_eq!(
            saved
                .wizard_metadata
                .providers
                .get(&WizardProviderCode::Codex)
                .unwrap()
                .applied_preset_revision,
            Some(1)
        );
    }

    #[test]
    fn home_reminder_ignores_opencode_ollama_and_missing_applied_revision() {
        let providers = WizardProviderCode::all()
            .iter()
            .map(|provider| EffortWizardProviderBootstrap {
                provider: *provider,
                available: *provider == WizardProviderCode::Codex,
                version: None,
                current_preset_revision: 1,
                applied_preset_revision: None,
                has_any_effort_setting: false,
                preset_update_available: false,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            effort_wizard_home_reminder(&providers),
            Some(EffortWizardHomeReminder::InitialSetup)
        );

        let mut configured = providers.clone();
        configured[0].has_any_effort_setting = true;
        assert_eq!(effort_wizard_home_reminder(&configured), None);

        let mut outdated = configured.clone();
        outdated[0].current_preset_revision = 2;
        outdated[0].applied_preset_revision = Some(1);
        outdated[0].preset_update_available = true;
        assert_eq!(
            effort_wizard_home_reminder(&outdated),
            Some(EffortWizardHomeReminder::PresetUpdate {
                providers: vec![WizardProviderCode::Codex]
            })
        );
        outdated[0].available = false;
        outdated[0].preset_update_available = false;
        assert_eq!(effort_wizard_home_reminder(&outdated), None);
    }

    #[test]
    fn apply_updates_checked_confirmed_tiers_and_advances_revision_atomically() {
        let mut settings = PedelecSettings::default();
        settings.default_provider = Some(ProviderCode::Ollama);
        settings.provider_settings.codex.efforts_args = EffortsArgs {
            low: vec!["-m".into(), "old-low".into()],
            default: vec!["-m".into(), "old-default".into()],
            high: vec!["-m".into(), "old-high".into()],
        };
        settings.provider_settings.opencode.efforts_args.default =
            vec!["--model".into(), "opencode/custom".into()];
        settings.provider_settings.ollama = OllamaProviderSettings {
            api_key: "ollama".into(),
            ..OllamaProviderSettings::default()
        };
        let current = settings.provider_settings.codex.efforts_args.clone();
        let (temp, runtime) = runtime_with_settings(settings);
        let manifest = bundled_effort_wizard_manifest().unwrap();
        let profiles = &manifest.providers[&WizardProviderCode::Codex].profiles;
        let saved = runtime
            .apply_effort_wizard_patch(patch_for(
                WizardProviderCode::Codex,
                current,
                EffortWizardConfirmedProfiles {
                    low: Some(profiles.low.clone()),
                    default: Some(profiles.default.clone()),
                    high: Some(profiles.high.clone()),
                },
                EffortWizardTierDecisions {
                    low: EffortWizardTierDecision::Update,
                    default: EffortWizardTierDecision::KeepCurrent,
                    high: EffortWizardTierDecision::Update,
                },
                true,
            ))
            .unwrap();
        assert_eq!(saved.provider_settings.codex.efforts_args.low, profiles.low);
        assert_eq!(
            saved.provider_settings.codex.efforts_args.default,
            vec!["-m", "old-default"]
        );
        assert_eq!(
            saved.provider_settings.codex.efforts_args.high,
            profiles.high
        );
        assert_eq!(
            saved.provider_settings.opencode.efforts_args.default,
            vec!["--model", "opencode/custom"]
        );
        assert_eq!(
            saved
                .wizard_metadata
                .providers
                .get(&WizardProviderCode::Codex)
                .unwrap()
                .applied_preset_revision,
            Some(1)
        );
        assert_eq!(
            read_settings_file(&temp.path().join("settings.json")).unwrap(),
            saved
        );
    }

    #[test]
    fn apply_partial_recommendation_can_advance_and_rejects_unconfirmed_update() {
        let settings = PedelecSettings::default();
        let current = settings.provider_settings.claude.efforts_args.clone();
        let (temp, runtime) = runtime_with_settings(settings);
        let profiles = &bundled_effort_wizard_manifest().unwrap().providers
            [&WizardProviderCode::Claude]
            .profiles;
        let result = runtime
            .apply_effort_wizard_patch(patch_for(
                WizardProviderCode::Claude,
                current.clone(),
                EffortWizardConfirmedProfiles {
                    low: Some(profiles.low.clone()),
                    default: Some(profiles.default.clone()),
                    high: None,
                },
                EffortWizardTierDecisions {
                    low: EffortWizardTierDecision::KeepCurrent,
                    default: EffortWizardTierDecision::Update,
                    high: EffortWizardTierDecision::KeepCurrent,
                },
                true,
            ))
            .unwrap();
        assert_eq!(
            result.provider_settings.claude.efforts_args.default,
            profiles.default
        );
        assert_eq!(
            result
                .wizard_metadata
                .providers
                .get(&WizardProviderCode::Claude)
                .unwrap()
                .applied_preset_revision,
            Some(1)
        );

        let before = read_settings_file(&temp.path().join("settings.json")).unwrap();
        let error = runtime
            .apply_effort_wizard_patch(patch_for(
                WizardProviderCode::Claude,
                before.provider_settings.claude.efforts_args.clone(),
                EffortWizardConfirmedProfiles {
                    low: Some(profiles.low.clone()),
                    default: Some(profiles.default.clone()),
                    high: None,
                },
                EffortWizardTierDecisions {
                    low: EffortWizardTierDecision::KeepCurrent,
                    default: EffortWizardTierDecision::KeepCurrent,
                    high: EffortWizardTierDecision::Update,
                },
                true,
            ))
            .unwrap_err();
        assert_eq!(error.code, error_codes::EFFORT_WIZARD_APPLY_INVALID);
        assert_eq!(
            read_settings_file(&temp.path().join("settings.json")).unwrap(),
            before
        );
    }

    #[test]
    fn stale_preview_rejects_the_entire_apply_without_writing() {
        let mut settings = PedelecSettings::default();
        settings.provider_settings.codex.efforts_args.default = vec!["-m".into(), "current".into()];
        settings.provider_settings.claude.efforts_args.default =
            vec!["--model".into(), "current".into()];
        let (temp, runtime) = runtime_with_settings(settings.clone());
        let codex_profiles = &bundled_effort_wizard_manifest().unwrap().providers
            [&WizardProviderCode::Codex]
            .profiles;
        let stale = EffortWizardProviderApplyPatch {
            provider: WizardProviderCode::Codex,
            preset_revision: 1,
            expected_current_efforts: EffortsArgs::default(),
            confirmed_recommendation: EffortWizardProviderRecommendation {
                provider: WizardProviderCode::Codex,
                preset_revision: 1,
                confirmed: EffortWizardConfirmedProfiles {
                    default: Some(codex_profiles.default.clone()),
                    ..Default::default()
                },
                deterministic_complete: true,
            },
            tier_decisions: EffortWizardTierDecisions {
                default: EffortWizardTierDecision::Update,
                ..Default::default()
            },
            can_advance_revision: true,
        };
        let claude_profiles = &bundled_effort_wizard_manifest().unwrap().providers
            [&WizardProviderCode::Claude]
            .profiles;
        let valid = EffortWizardProviderApplyPatch {
            provider: WizardProviderCode::Claude,
            preset_revision: 1,
            expected_current_efforts: settings.provider_settings.claude.efforts_args.clone(),
            confirmed_recommendation: EffortWizardProviderRecommendation {
                provider: WizardProviderCode::Claude,
                preset_revision: 1,
                confirmed: EffortWizardConfirmedProfiles {
                    default: Some(claude_profiles.default.clone()),
                    ..Default::default()
                },
                deterministic_complete: true,
            },
            tier_decisions: EffortWizardTierDecisions {
                default: EffortWizardTierDecision::Update,
                ..Default::default()
            },
            can_advance_revision: true,
        };
        let error = runtime
            .apply_effort_wizard_patch(EffortWizardApplyPatch {
                providers: vec![stale, valid],
            })
            .unwrap_err();
        assert_eq!(error.code, error_codes::EFFORT_WIZARD_SETTINGS_CHANGED);
        assert_eq!(
            read_settings_file(&temp.path().join("settings.json")).unwrap(),
            settings
        );
    }

    #[test]
    fn all_confirmed_tiers_can_keep_current_and_still_advance_revision() {
        let mut settings = PedelecSettings::default();
        settings.provider_settings.cursor.efforts_args.default =
            vec!["--model".into(), "manual".into()];
        let current = settings.provider_settings.cursor.efforts_args.clone();
        let (_temp, runtime) = runtime_with_settings(settings);
        let profiles = &bundled_effort_wizard_manifest().unwrap().providers
            [&WizardProviderCode::Cursor]
            .profiles;
        let saved = runtime
            .apply_effort_wizard_patch(patch_for(
                WizardProviderCode::Cursor,
                current.clone(),
                EffortWizardConfirmedProfiles {
                    low: Some(profiles.low.clone()),
                    default: Some(profiles.default.clone()),
                    high: Some(profiles.high.clone()),
                },
                EffortWizardTierDecisions::default(),
                true,
            ))
            .unwrap();
        assert_eq!(saved.provider_settings.cursor.efforts_args, current);
        assert_eq!(
            saved
                .wizard_metadata
                .providers
                .get(&WizardProviderCode::Cursor)
                .unwrap()
                .applied_preset_revision,
            Some(1)
        );
    }
}
