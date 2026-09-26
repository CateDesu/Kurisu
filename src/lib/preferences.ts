export const preferences = {
  get(key: string): string | null {
    try {
      return localStorage.getItem(key);
    } catch {
      return null;
    }
  },
  set(key: string, value: string) {
    try {
      localStorage.setItem(key, value);
    } catch {
    }
  },
  remove(key: string) {
    try {
      localStorage.removeItem(key);
    } catch {
    }
  },
  getJson(key: string): unknown {
    try {
      return JSON.parse(preferences.get(key) ?? "null");
    } catch {
      return null;
    }
  },
  setJson(key: string, value: unknown) {
    try {
      preferences.set(key, JSON.stringify(value));
    } catch {
    }
  },
};
