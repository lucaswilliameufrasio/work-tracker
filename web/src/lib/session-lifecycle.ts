type SessionEndedListener = () => void;

const sessionEndedListeners = new Set<SessionEndedListener>();

export function onSessionEnded(listener: SessionEndedListener): () => void {
  sessionEndedListeners.add(listener);
  return () => {
    sessionEndedListeners.delete(listener);
  };
}

export function notifySessionEnded(): void {
  for (const listener of sessionEndedListeners) {
    try {
      listener();
    } catch {
      continue;
    }
  }
}
