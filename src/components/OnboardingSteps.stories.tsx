import { Loader2Icon, CheckCircle2Icon, AlertCircleIcon, FolderIcon } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import {
  Field,
  FieldLabel,
  FieldDescription,
  FieldError,
} from '@/components/ui/field';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';

// ---------------------------------------------------------------------------
// ServerCheckStep — presentational mocks (the real component calls
// checkServerHealth() on mount, so we reproduce its JSX for each state)
// ---------------------------------------------------------------------------

function ServerCheckChecking() {
  return (
    <div className="flex flex-col items-center gap-6 text-center">
      <div className="flex flex-col items-center gap-4">
        <Loader2Icon className="h-12 w-12 animate-spin text-primary" />
        <div>
          <h2 className="text-xl font-semibold">Connecting to Server</h2>
          <p className="text-sm text-muted-foreground">Checking server health...</p>
        </div>
      </div>
    </div>
  );
}

function ServerCheckSuccess() {
  return (
    <div className="flex flex-col items-center gap-6 text-center">
      <div className="flex flex-col items-center gap-4">
        <CheckCircle2Icon className="h-12 w-12 text-green-500" />
        <div>
          <h2 className="text-xl font-semibold">Server Connected</h2>
          <p className="text-sm text-muted-foreground">
            Successfully connected to the Djinn server.
          </p>
        </div>
      </div>
    </div>
  );
}

function ServerCheckError() {
  return (
    <div className="flex flex-col items-center gap-6 text-center">
      <div className="flex flex-col items-center gap-4">
        <AlertCircleIcon className="h-12 w-12 text-destructive" />
        <div>
          <h2 className="text-xl font-semibold">Connection Failed</h2>
          <p className="text-sm text-muted-foreground">
            Could not connect to the server. Check that the backend is running.
          </p>
        </div>
        <Button variant="outline" onClick={() => {}}>
          Retry Connection
        </Button>
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// ProjectSetupStep — presentational mocks for each visual state
// ---------------------------------------------------------------------------

function ProjectSetupEmpty() {
  return (
    <div className="flex flex-col gap-6">
      <div className="text-center">
        <h2 className="text-2xl font-semibold">Set Up Your Project</h2>
        <p className="text-muted-foreground">
          Select a directory to register as your first project.
        </p>
      </div>

      <div className="flex flex-col gap-4">
        <Field>
          <FieldLabel>Project Directory</FieldLabel>
          <div className="flex gap-2">
            <Input placeholder="Select a directory..." value="" readOnly className="flex-1" />
            <Button variant="secondary">
              <FolderIcon className="mr-2 h-4 w-4" />
              Browse
            </Button>
          </div>
          <FieldDescription>Choose the root directory for your project.</FieldDescription>
        </Field>

        <Button disabled className="w-full">
          Register Project
        </Button>
      </div>
    </div>
  );
}

function ProjectSetupWithPath() {
  return (
    <div className="flex flex-col gap-6">
      <div className="text-center">
        <h2 className="text-2xl font-semibold">Set Up Your Project</h2>
        <p className="text-muted-foreground">
          Select a directory to register as your first project.
        </p>
      </div>

      <div className="flex flex-col gap-4">
        <Field>
          <FieldLabel>Project Directory</FieldLabel>
          <div className="flex gap-2">
            <Input
              placeholder="Select a directory..."
              value="/home/user/projects/my-app"
              readOnly
              className="flex-1"
            />
            <Button variant="secondary">
              <FolderIcon className="mr-2 h-4 w-4" />
              Browse
            </Button>
          </div>
          <FieldDescription>Choose the root directory for your project.</FieldDescription>
        </Field>

        <Field>
          <FieldLabel>Project Name</FieldLabel>
          <Input placeholder="Enter project name..." value="my-app" readOnly />
          <FieldDescription>This name will be used to identify your project.</FieldDescription>
        </Field>

        <Button className="w-full">Register Project</Button>
      </div>
    </div>
  );
}

function ProjectSetupRegistered() {
  return (
    <div className="flex flex-col gap-6">
      <div className="text-center">
        <h2 className="text-2xl font-semibold">Set Up Your Project</h2>
        <p className="text-muted-foreground">
          Select a directory to register as your first project.
        </p>
      </div>

      <div className="flex flex-col gap-4">
        <Field>
          <FieldLabel>Project Directory</FieldLabel>
          <div className="flex gap-2">
            <Input
              placeholder="Select a directory..."
              value="/home/user/projects/my-app"
              readOnly
              className="flex-1"
            />
            <Button variant="secondary">
              <FolderIcon className="mr-2 h-4 w-4" />
              Browse
            </Button>
          </div>
          <FieldDescription>Choose the root directory for your project.</FieldDescription>
        </Field>

        <Field>
          <FieldLabel>Project Name</FieldLabel>
          <Input placeholder="Enter project name..." value="my-app" readOnly />
          <FieldDescription>This name will be used to identify your project.</FieldDescription>
        </Field>

        <div className="flex items-center gap-2 rounded-md bg-green-500/10 p-3 text-sm text-green-600">
          <CheckCircle2Icon className="h-4 w-4 flex-shrink-0" />
          <span>Project registered successfully!</span>
        </div>

        <Button disabled className="w-full">
          <CheckCircle2Icon className="mr-2 h-4 w-4" />
          Registered
        </Button>
      </div>
    </div>
  );
}

function ProjectSetupError() {
  return (
    <div className="flex flex-col gap-6">
      <div className="text-center">
        <h2 className="text-2xl font-semibold">Set Up Your Project</h2>
        <p className="text-muted-foreground">
          Select a directory to register as your first project.
        </p>
      </div>

      <div className="flex flex-col gap-4">
        <Field>
          <FieldLabel>Project Directory</FieldLabel>
          <div className="flex gap-2">
            <Input
              placeholder="Select a directory..."
              value="/home/user/projects/my-app"
              readOnly
              className="flex-1"
            />
            <Button variant="secondary">
              <FolderIcon className="mr-2 h-4 w-4" />
              Browse
            </Button>
          </div>
          <FieldDescription>Choose the root directory for your project.</FieldDescription>
        </Field>

        <Field>
          <FieldLabel>Project Name</FieldLabel>
          <Input placeholder="Enter project name..." value="my-app" readOnly />
          <FieldDescription>This name will be used to identify your project.</FieldDescription>
        </Field>

        <div className="flex items-center gap-2 rounded-md bg-destructive/10 p-3 text-sm text-destructive">
          <AlertCircleIcon className="h-4 w-4 flex-shrink-0" />
          <span>Failed to register project: directory not found</span>
        </div>

        <Button className="w-full">Register Project</Button>
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// ProviderSetupStep — presentational mocks for each visual state
// ---------------------------------------------------------------------------

function ProviderSetupLoading() {
  return (
    <div className="flex flex-col items-center gap-4 text-center">
      <Loader2Icon className="h-8 w-8 animate-spin text-primary" />
      <p className="text-sm text-muted-foreground">Loading providers...</p>
    </div>
  );
}

function ProviderSetupLoadError() {
  return (
    <div className="flex flex-col items-center gap-4 text-center">
      <AlertCircleIcon className="h-12 w-12 text-destructive" />
      <div>
        <h2 className="text-xl font-semibold">Failed to Load Providers</h2>
        <p className="text-sm text-muted-foreground">
          Could not fetch provider catalog from server.
        </p>
      </div>
      <Button variant="outline" onClick={() => {}}>
        Retry
      </Button>
    </div>
  );
}

function ProviderSetupSelectProvider() {
  return (
    <div className="flex flex-col gap-6">
      <div className="text-center">
        <h2 className="text-2xl font-semibold">Configure AI Provider</h2>
        <p className="text-muted-foreground">
          Set up your AI provider to get started with Djinn.
        </p>
      </div>

      <div className="flex flex-col gap-4">
        <Field>
          <FieldLabel>Provider</FieldLabel>
          <Select>
            <SelectTrigger className="w-full">
              <SelectValue placeholder="Select a provider" />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value="anthropic">Anthropic</SelectItem>
              <SelectItem value="openai">OpenAI</SelectItem>
              <SelectItem value="google">Google AI</SelectItem>
            </SelectContent>
          </Select>
          <FieldDescription>
            Choose your AI provider from the available options.
          </FieldDescription>
        </Field>
      </div>
    </div>
  );
}

function ProviderSetupApiKey() {
  return (
    <div className="flex flex-col gap-6">
      <div className="text-center">
        <h2 className="text-2xl font-semibold">Configure AI Provider</h2>
        <p className="text-muted-foreground">
          Set up your AI provider to get started with Djinn.
        </p>
      </div>

      <div className="flex flex-col gap-4">
        <Field>
          <FieldLabel>Provider</FieldLabel>
          <Select value="anthropic">
            <SelectTrigger className="w-full">
              <SelectValue>Anthropic</SelectValue>
            </SelectTrigger>
            <SelectContent>
              <SelectItem value="anthropic">Anthropic</SelectItem>
              <SelectItem value="openai">OpenAI</SelectItem>
            </SelectContent>
          </Select>
          <FieldDescription>
            Choose your AI provider from the available options.
          </FieldDescription>
        </Field>

        <Field>
          <FieldLabel>API Key</FieldLabel>
          <div className="flex gap-2">
            <Input
              type="password"
              placeholder="Enter your API key"
              value="sk-ant-api03-xxxxx"
              readOnly
              className="flex-1"
            />
            <Button variant="secondary">Validate</Button>
          </div>
          <FieldDescription>
            Your API key will be securely stored and never shared.
          </FieldDescription>
        </Field>

        <Button disabled={true} className="w-full">
          Continue
        </Button>
      </div>
    </div>
  );
}

function ProviderSetupValidated() {
  return (
    <div className="flex flex-col gap-6">
      <div className="text-center">
        <h2 className="text-2xl font-semibold">Configure AI Provider</h2>
        <p className="text-muted-foreground">
          Set up your AI provider to get started with Djinn.
        </p>
      </div>

      <div className="flex flex-col gap-4">
        <Field>
          <FieldLabel>Provider</FieldLabel>
          <Select value="anthropic">
            <SelectTrigger className="w-full">
              <SelectValue>Anthropic</SelectValue>
            </SelectTrigger>
            <SelectContent>
              <SelectItem value="anthropic">Anthropic</SelectItem>
              <SelectItem value="openai">OpenAI</SelectItem>
            </SelectContent>
          </Select>
          <FieldDescription>
            Choose your AI provider from the available options.
          </FieldDescription>
        </Field>

        <Field>
          <FieldLabel>API Key</FieldLabel>
          <div className="flex gap-2">
            <Input
              type="password"
              placeholder="Enter your API key"
              value="sk-ant-api03-xxxxx"
              readOnly
              className="flex-1"
            />
            <Button variant="secondary">Validate</Button>
          </div>
          <FieldDescription>
            Your API key will be securely stored and never shared.
          </FieldDescription>
          <div className="flex items-center gap-2 text-sm text-green-500">
            <CheckCircle2Icon className="h-4 w-4" />
            <span>API key is valid</span>
          </div>
        </Field>

        <Button className="w-full">Continue</Button>
      </div>
    </div>
  );
}

function ProviderSetupValidationError() {
  return (
    <div className="flex flex-col gap-6">
      <div className="text-center">
        <h2 className="text-2xl font-semibold">Configure AI Provider</h2>
        <p className="text-muted-foreground">
          Set up your AI provider to get started with Djinn.
        </p>
      </div>

      <div className="flex flex-col gap-4">
        <Field>
          <FieldLabel>Provider</FieldLabel>
          <Select value="openai">
            <SelectTrigger className="w-full">
              <SelectValue>OpenAI</SelectValue>
            </SelectTrigger>
            <SelectContent>
              <SelectItem value="anthropic">Anthropic</SelectItem>
              <SelectItem value="openai">OpenAI</SelectItem>
            </SelectContent>
          </Select>
          <FieldDescription>
            Choose your AI provider from the available options.
          </FieldDescription>
        </Field>

        <Field>
          <FieldLabel>API Key</FieldLabel>
          <div className="flex gap-2">
            <Input
              type="password"
              placeholder="Enter your API key"
              value="sk-invalid-key"
              readOnly
              className="flex-1"
            />
            <Button variant="secondary">Validate</Button>
          </div>
          <FieldDescription>
            Your API key will be securely stored and never shared.
          </FieldDescription>
          <FieldError>Invalid API key: authentication failed</FieldError>
        </Field>

        <Button disabled={true} className="w-full">
          Continue
        </Button>
      </div>
    </div>
  );
}

function ProviderSetupOAuth() {
  return (
    <div className="flex flex-col gap-6">
      <div className="text-center">
        <h2 className="text-2xl font-semibold">Configure AI Provider</h2>
        <p className="text-muted-foreground">
          Set up your AI provider to get started with Djinn.
        </p>
      </div>

      <div className="flex flex-col gap-4">
        <Field>
          <FieldLabel>Provider</FieldLabel>
          <Select value="anthropic">
            <SelectTrigger className="w-full">
              <SelectValue>Anthropic</SelectValue>
            </SelectTrigger>
            <SelectContent>
              <SelectItem value="anthropic">Anthropic</SelectItem>
              <SelectItem value="openai">OpenAI</SelectItem>
            </SelectContent>
          </Select>
          <FieldDescription>
            Choose your AI provider from the available options.
          </FieldDescription>
        </Field>

        <Button className="w-full">Connect with OAuth</Button>

        <div className="flex items-center gap-3 text-xs text-muted-foreground">
          <div className="h-px flex-1 bg-border" />
          <span>or enter an API key</span>
          <div className="h-px flex-1 bg-border" />
        </div>

        <Field>
          <FieldLabel>API Key</FieldLabel>
          <div className="flex gap-2">
            <Input
              type="password"
              placeholder="Enter your API key"
              className="flex-1"
            />
            <Button variant="secondary" disabled>
              Validate
            </Button>
          </div>
          <FieldDescription>
            Your API key will be securely stored and never shared.
          </FieldDescription>
        </Field>
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Story exports
// ---------------------------------------------------------------------------

export default {
  title: 'Onboarding/Steps',
};

// -- ServerCheckStep --

export const ServerCheckStepChecking = {
  name: 'ServerCheckStep / Checking',
  render: () => <ServerCheckChecking />,
};

export const ServerCheckStepSuccess = {
  name: 'ServerCheckStep / Success',
  render: () => <ServerCheckSuccess />,
};

export const ServerCheckStepError = {
  name: 'ServerCheckStep / Error',
  render: () => <ServerCheckError />,
};

// -- ProjectSetupStep --

export const ProjectSetupStepEmpty = {
  name: 'ProjectSetupStep / Empty',
  render: () => <ProjectSetupEmpty />,
};

export const ProjectSetupStepWithPath = {
  name: 'ProjectSetupStep / With Path',
  render: () => <ProjectSetupWithPath />,
};

export const ProjectSetupStepRegistered = {
  name: 'ProjectSetupStep / Registered',
  render: () => <ProjectSetupRegistered />,
};

export const ProjectSetupStepError = {
  name: 'ProjectSetupStep / Error',
  render: () => <ProjectSetupError />,
};

// -- ProviderSetupStep --

export const ProviderSetupStepLoading = {
  name: 'ProviderSetupStep / Loading',
  render: () => <ProviderSetupLoading />,
};

export const ProviderSetupStepLoadError = {
  name: 'ProviderSetupStep / Load Error',
  render: () => <ProviderSetupLoadError />,
};

export const ProviderSetupStepSelectProvider = {
  name: 'ProviderSetupStep / Select Provider',
  render: () => <ProviderSetupSelectProvider />,
};

export const ProviderSetupStepApiKey = {
  name: 'ProviderSetupStep / API Key Entered',
  render: () => <ProviderSetupApiKey />,
};

export const ProviderSetupStepValidated = {
  name: 'ProviderSetupStep / Validated',
  render: () => <ProviderSetupValidated />,
};

export const ProviderSetupStepValidationError = {
  name: 'ProviderSetupStep / Validation Error',
  render: () => <ProviderSetupValidationError />,
};

export const ProviderSetupStepOAuth = {
  name: 'ProviderSetupStep / OAuth Provider',
  render: () => <ProviderSetupOAuth />,
};
