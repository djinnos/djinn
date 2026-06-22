export type { BlockProps } from "./types";
export { DataModelBlock } from "./DataModelBlock";
export { ApiEndpointBlock } from "./ApiEndpointBlock";
export { DecisionsBlock } from "./DecisionsBlock";
export { FileTreeBlock } from "./FileTreeBlock";
export { QuestionFormBlock } from "./QuestionFormBlock";
export { RichText } from "./RichText";
export { Diagram } from "./Diagram";
export { AnnotatedCode } from "./AnnotatedCode";
export { DiffBlock } from "./DiffBlock";
export { CalloutBlock } from "./CalloutBlock";
export { ChecklistBlock } from "./ChecklistBlock";
export { JsonExplorerBlock, JsonTree } from "./JsonExplorerBlock";
export { OpenApiSpecBlock } from "./OpenApiSpecBlock";
export { TabsBlock } from "./TabsBlock";
export { ColumnsBlock } from "./ColumnsBlock";
export { HtmlBlock } from "./HtmlBlock";
export { WireframeBlock } from "./WireframeBlock";
export { SandboxedHtmlFrame } from "./SandboxedHtmlFrame";
export {
  buildWireframeSrcDoc,
  resolveWireframeSurface,
  WIREFRAME_SURFACES,
  DEFAULT_WIREFRAME_SURFACE,
} from "./wireframeHtml";
export type { WireframeSurface } from "./wireframeHtml";
export {
  renderWireframeIconHtml,
  WIREFRAME_ICON_NAMES,
} from "./wireframeIcons";
export {
  sanitizeBlockHtml,
  buildIframeSrcDoc,
  SANDBOX_VALUE,
  IFRAME_CSP,
} from "./sandboxedHtml";

// Reusable line-anchored annotation engine (pure parsing + React rail UI). The
// `annotated-code` block consumes it; `diff` and future blocks can adopt it.
export {
  parseLineRange,
  parseAnnotationsAttr,
  coerceAnnotation,
  resolveAnnotations,
  buildLineMarkerMap,
  rangeLabel,
  hasResolvedAnnotations,
} from "./annotations";
export type {
  RawAnnotation,
  LineRange,
  ResolvedAnnotation,
} from "./annotations";
export {
  AnnotationGutterMarker,
  AnnotationCard,
  AnnotationHiddenStack,
  AnnotationHoverCard,
} from "./annotationRail";
export {
  useAnnotationHover,
  anchorFromElements,
  resolveAnnotationHoverCardPosition,
} from "./annotationRail.helpers";
export type {
  AnnotationAnchor,
  AnnotationSide,
} from "./annotationRail.helpers";
export { BlockRenderer } from "./BlockRenderer";
export type { BlockRendererProps } from "./BlockRenderer";
export { ProposalBlocks } from "./ProposalBlocks";
export type { ProposalBlocksProps } from "./ProposalBlocks";
export { MAX_BLOCK_DEPTH, useBlockDepth } from "./blockDepth";
export { parseMdxBody, isPascalCaseTag, extractBlockTags } from "./parseMdxBody";
export {
  PROPOSAL_BLOCK_REGISTRY,
  PROPOSAL_BLOCK_DEFINITIONS_BY_TYPE,
  PROPOSAL_BLOCK_DEFINITIONS_BY_TAG,
  getProposalBlockDefinition,
  getProposalBlockDefinitionByTag,
  extractProposalBlockIds,
} from "./blockRegistry";
export type {
  ExtractedProposalBlockId,
  ProposalBlockDefinition,
  ProposalBlockFieldMap,
  ProposalBlockFieldSchema,
  ProposalBlockRegistry,
} from "./blockRegistry";
