use std::fs;

let path = "/workspace/.tmp0oOkeU/server/src/server/usage_analytics.rs";
let content = fs::read_to_string(path).unwrap();
let replacements = [
    ("#[derive(Serialize)]\nstruct UsageKpiDto", "#[derive(Serialize, JsonSchema)]\nstruct UsageKpiDto"),
    ("#[derive(Serialize)]\nstruct UsageKpiPartDto", "#[derive(Serialize, JsonSchema)]\nstruct UsageKpiPartDto"),
    ("#[derive(Serialize)]\nstruct SeriesPointDto", "#[derive(Serialize, JsonSchema)]\nstruct SeriesPointDto"),
    ("#[derive(Serialize)]\nstruct BreakdownRowDto", "#[derive(Serialize, JsonSchema)]\nstruct BreakdownRowDto"),
    ("#[derive(Serialize)]\nstruct BreakdownsDto", "#[derive(Serialize, JsonSchema)]\nstruct BreakdownsDto"),
    ("#[derive(Serialize)]\nstruct ModelEffectivenessDto", "#[derive(Serialize, JsonSchema)]\nstruct ModelEffectivenessDto"),
    ("#[derive(Serialize)]\nstruct ProjectModelCellDto", "#[derive(Serialize, JsonSchema)]\nstruct ProjectModelCellDto"),
    ("#[derive(Serialize)]\nstruct UsageResponse", "#[derive(Serialize, JsonSchema)]\nstruct UsageResponse"),
];
let mut updated = content;
for (old, new) in replacements {
    updated = updated.replace(old, new);
}
fs::write(path, updated).unwrap();
println!("done");
