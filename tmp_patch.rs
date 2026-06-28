use std::fs;

fn main() {
    let path = "/workspace/.tmp0oOkeU/server/src/server/usage_analytics.rs";
    let mut content = fs::read_to_string(path).unwrap();
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
    for (old, new) in replacements {
        content = content.replace(old, new);
    }
    fs::write(path, content).unwrap();
    println!("done");
}
