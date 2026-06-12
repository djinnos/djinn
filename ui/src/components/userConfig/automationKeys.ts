// react-query keys are scoped by the automation user's id so this component's
// cache never collides with the current-user singletons elsewhere in the app.
export const automationKeys = {
  connectedProviders: (id: string) => ["automation", id, "connected-providers"] as const,
  catalog: (id: string) => ["automation", id, "catalog"] as const,
  connectedModels: (id: string) => ["automation", id, "connected-models"] as const,
  modelSelection: (id: string) => ["automation", id, "model-selection"] as const,
};
