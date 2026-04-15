import { create } from "zustand";
import {
  authGetState,
  authLogin,
  authLogout,
  type AuthState,
  type AuthUser,
} from "@/api/auth";

interface AuthStore {
  isAuthenticated: boolean;
  user: AuthUser | null;
  isLoading: boolean;
  error: string | null;

  /** Fetch auth state from the server. */
  fetchState: () => Promise<void>;
  /** Update auth state directly. */
  setState: (state: AuthState) => void;
  /** Initiate login flow. */
  login: () => Promise<void>;
  /** Log out and clear state. */
  logout: () => Promise<void>;
}

export const useAuthStore = create<AuthStore>((set) => ({
  isAuthenticated: false,
  user: null,
  isLoading: true,
  error: null,

  fetchState: async () => {
    try {
      const state = await authGetState();
      set({
        isAuthenticated: state.isAuthenticated,
        user: state.user,
        isLoading: false,
        error: null,
      });
    } catch (e) {
      set({ isLoading: false, error: String(e) });
    }
  },

  setState: (state) => {
    set({
      isAuthenticated: state.isAuthenticated,
      user: state.user,
      isLoading: false,
      error: null,
    });
  },

  login: async () => {
    try {
      set({ error: null });
      await authLogin();
    } catch (e) {
      set({ error: String(e) });
    }
  },

  logout: async () => {
    try {
      await authLogout();
      set({ isAuthenticated: false, user: null, error: null });
    } catch (e) {
      set({ error: String(e) });
    }
  },
}));
