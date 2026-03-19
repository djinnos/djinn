import { useEffect } from 'react';
import { useAuthStore } from '@/stores/authStore';
import { AuthGate } from './AuthGate';

/**
 * Wrapper that seeds authStore state before rendering AuthGate.
 * AuthGate listens for Tauri events internally, but in Storybook those
 * never fire, so we drive the store directly.
 */
const AuthGateState = ({
  isAuthenticated,
  isLoading,
  error,
}: {
  isAuthenticated: boolean;
  isLoading: boolean;
  error?: string | null;
}) => {
  useEffect(() => {
    useAuthStore.setState({
      isAuthenticated,
      isLoading,
      error: error ?? null,
      user: isAuthenticated ? { email: 'user@example.com', sub: 'user-1' } : null,
    });
  }, [isAuthenticated, isLoading, error]);

  return (
    <AuthGate>
      <div className="flex min-h-screen items-center justify-center bg-background text-foreground">
        <div className="rounded-lg border p-8 text-center">
          <h1 className="text-2xl font-semibold">Dashboard content</h1>
          <p className="text-sm text-muted-foreground">
            You are signed in and can see this content.
          </p>
        </div>
      </div>
    </AuthGate>
  );
};

export default {
  title: 'Auth/AuthGate',
};

export const Loading = {
  name: 'Loading',
  render: () => <AuthGateState isAuthenticated={false} isLoading={true} />,
};

export const SignInRequired = {
  name: 'Sign In Required',
  render: () => <AuthGateState isAuthenticated={false} isLoading={false} />,
};

export const SignInWithError = {
  name: 'Sign In With Error',
  render: () => (
    <AuthGateState
      isAuthenticated={false}
      isLoading={false}
      error="Your session has expired. Please sign in again."
    />
  ),
};

export const Authenticated = {
  name: 'Authenticated',
  render: () => <AuthGateState isAuthenticated={true} isLoading={false} />,
};
