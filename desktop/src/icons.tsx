export function IconSpeaker() {
  return (
    <svg width="20" height="20" viewBox="0 0 24 24" fill="currentColor" aria-hidden="true">
      <path d="M11 5 6 9H3v6h3l5 4V5zm2.5 3.5a4.5 4.5 0 0 1 0 7" fill="none" stroke="currentColor" strokeWidth="2" />
    </svg>
  );
}

export function IconHash() {
  return (
    <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" aria-hidden="true">
      <path d="M5 9h14M5 15h14M9 4 7 20M17 4l-2 16" />
    </svg>
  );
}

export function IconHome() {
  return (
    <svg width="26" height="26" viewBox="0 0 24 24" fill="currentColor" aria-hidden="true">
      <path d="M4 10v9h6v-5h4v5h6v-9l-8-6-8 6z" />
    </svg>
  );
}

export function IconMic({ muted }: { muted: boolean }) {
  return (
    <svg width="20" height="20" viewBox="0 0 24 24" fill="currentColor" aria-hidden="true">
      <path d="M12 14a3 3 0 0 0 3-3V6a3 3 0 0 0-6 0v5a3 3 0 0 0 3 3zm5-3a5 5 0 0 1-10 0H5a7 7 0 0 0 6 6.9V21h2v-3.1A7 7 0 0 0 19 11h-2z" />
      {muted ? <path d="M4 4l16 16" fill="none" stroke="currentColor" strokeWidth="2" /> : null}
    </svg>
  );
}

export function IconHeadphones({ deaf }: { deaf: boolean }) {
  return (
    <svg width="20" height="20" viewBox="0 0 24 24" fill="currentColor" aria-hidden="true">
      <path d="M12 3a8 8 0 0 0-8 8v6a2 2 0 0 0 2 2h2v-7H6v-1a6 6 0 0 1 12 0v1h-2v7h2a2 2 0 0 0 2-2v-6a8 8 0 0 0-8-8z" />
      {deaf ? <path d="M4 4l16 16" fill="none" stroke="currentColor" strokeWidth="2" /> : null}
    </svg>
  );
}

export function IconLeave() {
  return (
    <svg width="20" height="20" viewBox="0 0 24 24" fill="currentColor" aria-hidden="true">
      <path d="M7 4h6a2 2 0 0 1 2 2v2h-2V6H7v12h6v-2h2v2a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6a2 2 0 0 1 2-2zm8.3 4.3 1.4 1.4L15.4 11H22v2h-6.6l1.3 1.3-1.4 1.4L11.6 12l3.7-3.7z" />
    </svg>
  );
}

export function IconLog() {
  return (
    <svg width="20" height="20" viewBox="0 0 24 24" fill="currentColor" aria-hidden="true">
      <path d="M5 4h10l4 4v12a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V6a2 2 0 0 1 2-2zm8 1.5V9h3.5L13 5.5zM7 12h10v1.6H7V12zm0 3.2h10v1.6H7v-1.6zm0 3.2h7V20H7v-1.6z" />
    </svg>
  );
}

export function IconGear() {
  return (
    <svg width="20" height="20" viewBox="0 0 24 24" fill="currentColor" aria-hidden="true">
      <path d="M19.1 12.9a7.7 7.7 0 0 0 .1-1.8l2-1.5-2-3.4-2.4 1a7.8 7.8 0 0 0-1.6-.9l-.4-2.5h-4l-.4 2.5c-.5.2-1.1.5-1.6.9l-2.4-1-2 3.4 2 1.5a7.7 7.7 0 0 0 0 1.8l-2 1.5 2 3.4 2.4-1c.5.4 1 .7 1.6.9l.4 2.5h4l.4-2.5c.5-.2 1.1-.5 1.6-.9l2.4 1 2-3.4-2-1.5zM12 15.5A3.5 3.5 0 1 1 12 8a3.5 3.5 0 0 1 0 7.5z" />
    </svg>
  );
}
