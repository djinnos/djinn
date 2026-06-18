// react-query keys are scoped by the target user's id so this component's
// cache never collides with the current-user singletons elsewhere in the app.
export const userConfigKeys = {
  connectedProviders: (id: string) => ["user-config", id, "connected-providers"] as const,
  catalog: (id: string) => ["user-config", id, "catalog"] as const,
  connectedModels: (id: string) => ["user-config", id, "connected-models"] as const,
  modelSelection: (id: string) => ["user-config", id, "model-selection"] as const,
};
