<template>
  <!-- The app icon, inline so the server-rendered header shows it at once. -->
  <svg viewBox="82 82 860 860" width="40" height="40" aria-hidden="true">
    <defs>
      <linearGradient id="brand-mark-background" x1="0" y1="0" x2="1" y2="1">
        <stop offset="0" stop-color="#1F2A44" />
        <stop offset="1" stop-color="#0B1020" />
      </linearGradient>
      <linearGradient id="brand-mark-stripe" x1="0" y1="0" x2="1" y2="0">
        <stop offset="0" stop-color="#3574F0" />
        <stop offset="0.55" stop-color="#9B59FF" />
        <stop offset="1" stop-color="#FF7A45" />
      </linearGradient>
    </defs>
    <path
      d="M382 82 H642 C747 82 799 82 839 102 C875 120 904 149 922 185 C942 225 942 277 942 382 V642 C942 747 942 799 922 839 C904 875 875 904 839 922 C799 942 747 942 642 942 H382 C277 942 225 942 185 922 C149 904 120 875 102 839 C82 799 82 747 82 642 V382 C82 277 82 225 102 185 C120 149 149 120 185 102 C225 82 277 82 382 82 Z"
      fill="url(#brand-mark-background)"
    />
    <rect x="238" y="292" width="548" height="56" rx="28" fill="url(#brand-mark-stripe)" />
    <text
      x="512"
      y="690"
      text-anchor="middle"
      font-family="JetBrains Mono, Menlo, monospace"
      font-weight="700"
      font-size="300"
      letter-spacing="-12"
      fill="#FFFFFF"
    >
      evb
    </text>
    <rect x="238" y="738" width="190" height="40" rx="20" fill="#FFFFFF" opacity="0.85" />
  </svg>
</template>
