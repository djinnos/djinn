import { Component, ErrorInfo, ReactNode } from 'react';
import { Button } from '@/components/ui/button';
import { Copy, Check } from 'lucide-react';

interface Props {
  children: ReactNode;
}

interface State {
  hasError: boolean;
  error: Error | null;
  errorInfo: ErrorInfo | null;
  copied: boolean;
  showDetails: boolean;
}

export class ErrorBoundary extends Component<Props, State> {
  state: State = { hasError: false, error: null, errorInfo: null, copied: false, showDetails: false };

  static getDerivedStateFromError(error: Error): Partial<State> {
    return { hasError: true, error };
  }

  componentDidCatch(error: Error, errorInfo: ErrorInfo) {
    this.setState({ error, errorInfo });
  }

  handleRetry = () => {
    this.setState({ hasError: false, error: null, errorInfo: null, copied: false, showDetails: false });
  };

  formatErrorReport = (): string => {
    const { error, errorInfo } = this.state;
    const lines = [
      `**Error:** ${error?.message ?? 'Unknown error'}`,
      '',
      `**Stack:**`,
      '```',
      error?.stack ?? 'No stack trace available',
      '```',
    ];
    if (errorInfo?.componentStack) {
      lines.push('', `**Component Stack:**`, '```', errorInfo.componentStack.trim(), '```');
    }
    lines.push('', `**URL:** ${window.location.href}`, `**User Agent:** ${navigator.userAgent}`);
    return lines.join('\n');
  };

  handleCopy = async () => {
    try {
      await navigator.clipboard.writeText(this.formatErrorReport());
      this.setState({ copied: true });
      setTimeout(() => this.setState({ copied: false }), 2000);
    } catch {
      // Fallback for environments without clipboard API
      const textarea = document.createElement('textarea');
      textarea.value = this.formatErrorReport();
      document.body.appendChild(textarea);
      textarea.select();
      document.execCommand('copy');
      document.body.removeChild(textarea);
      this.setState({ copied: true });
      setTimeout(() => this.setState({ copied: false }), 2000);
    }
  };

  render() {
    if (this.state.hasError) {
      const { error, errorInfo, copied, showDetails } = this.state;

      return (
        <div className="flex min-h-screen items-center justify-center bg-background p-6">
          <div className="w-full max-w-lg rounded-lg border border-border bg-card p-6 text-center">
            <h2 className="text-lg font-semibold">Something went wrong</h2>
            <p className="mt-2 text-sm text-muted-foreground">
              An unexpected error occurred while rendering this view.
            </p>
            <div className="mt-4 flex items-center justify-center gap-2">
              <Button onClick={this.handleRetry}>Retry</Button>
              <Button variant="outline" onClick={this.handleCopy}>
                {copied ? <Check className="mr-1.5 h-4 w-4" /> : <Copy className="mr-1.5 h-4 w-4" />}
                {copied ? 'Copied!' : 'Copy Error'}
              </Button>
            </div>
            <button
              className="mt-3 text-xs text-muted-foreground underline-offset-2 hover:underline"
              onClick={() => this.setState({ showDetails: !showDetails })}
            >
              {showDetails ? 'Hide details' : 'Show details'}
            </button>
            {showDetails && (
              <div className="mt-3 max-h-64 overflow-auto rounded border border-border bg-background p-3 text-left">
                <p className="text-xs font-medium text-destructive">{error?.message}</p>
                {error?.stack && (
                  <pre className="mt-2 whitespace-pre-wrap text-[11px] text-muted-foreground">
                    {error.stack}
                  </pre>
                )}
                {errorInfo?.componentStack && (
                  <pre className="mt-2 whitespace-pre-wrap text-[11px] text-muted-foreground">
                    {errorInfo.componentStack.trim()}
                  </pre>
                )}
              </div>
            )}
          </div>
        </div>
      );
    }

    return this.props.children;
  }
}
