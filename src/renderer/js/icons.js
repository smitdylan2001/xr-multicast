'use strict';

(function () {
  const p = (d) => `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${d}</svg>`;
  const ICONS = {
    play: p('<polygon points="6 4 20 12 6 20 6 4" fill="currentColor"/>'),
    stop: p('<rect x="6" y="6" width="12" height="12" rx="1.5" fill="currentColor"/>'),
    radar: p('<path d="M12 12 19 5"/><path d="M20.2 9.5A8.5 8.5 0 1 1 14.5 3.8"/><path d="M16 11.2A4 4 0 1 1 12.8 8"/>'),
    plus: p('<path d="M12 5v14M5 12h14"/>'),
    grid: p('<rect x="3" y="3" width="7" height="7" rx="1"/><rect x="14" y="3" width="7" height="7" rx="1"/><rect x="3" y="14" width="7" height="7" rx="1"/><rect x="14" y="14" width="7" height="7" rx="1"/>'),
    help: p('<circle cx="12" cy="12" r="9.5"/><path d="M9.3 9a2.8 2.8 0 0 1 5.4 1c0 1.9-2.7 2.5-2.7 4"/><circle cx="12" cy="17.3" r=".6" fill="currentColor"/>'),
    gear: p('<circle cx="12" cy="12" r="3"/><path d="M19.4 15a1.7 1.7 0 0 0 .3 1.8l.1.1a2 2 0 1 1-2.8 2.8l-.1-.1a1.7 1.7 0 0 0-1.8-.3 1.7 1.7 0 0 0-1 1.5V21a2 2 0 1 1-4 0v-.1a1.7 1.7 0 0 0-1.1-1.5 1.7 1.7 0 0 0-1.8.3l-.1.1a2 2 0 1 1-2.8-2.8l.1-.1a1.7 1.7 0 0 0 .3-1.8 1.7 1.7 0 0 0-1.5-1H3a2 2 0 1 1 0-4h.1a1.7 1.7 0 0 0 1.5-1.1 1.7 1.7 0 0 0-.3-1.8l-.1-.1a2 2 0 1 1 2.8-2.8l.1.1a1.7 1.7 0 0 0 1.8.3H9a1.7 1.7 0 0 0 1-1.5V3a2 2 0 1 1 4 0v.1a1.7 1.7 0 0 0 1 1.5 1.7 1.7 0 0 0 1.8-.3l.1-.1a2 2 0 1 1 2.8 2.8l-.1.1a1.7 1.7 0 0 0-.3 1.8V9a1.7 1.7 0 0 0 1.5 1H21a2 2 0 1 1 0 4h-.1a1.7 1.7 0 0 0-1.5 1z"/>'),
    usb: p('<path d="M12 2v14"/><path d="m9 5 3-3 3 3"/><circle cx="12" cy="19" r="2.5"/><path d="M12 12 7 9.5V7"/><path d="m12 14 5-2.5V9"/><rect x="5.6" y="5" width="2.8" height="2.4"/><circle cx="17" cy="8" r="1.3"/>'),
    wifi: p('<path d="M2 8.8a15 15 0 0 1 20 0"/><path d="M5 12.6a10.5 10.5 0 0 1 14 0"/><path d="M8.5 16.4a5.5 5.5 0 0 1 7 0"/><circle cx="12" cy="20" r="1" fill="currentColor"/>'),
    check: p('<path d="M20 6 9 17l-5-5"/>'),
    shield: p('<path d="M12 22s8-4 8-10V5l-8-3-8 3v7c0 6 8 10 8 10z"/><path d="m9 12 2 2 4-4"/>'),
    alert: p('<path d="M10.3 3.9 1.8 18a2 2 0 0 0 1.7 3h17a2 2 0 0 0 1.7-3L13.7 3.9a2 2 0 0 0-3.4 0z"/><path d="M12 9v4"/><circle cx="12" cy="17" r=".6" fill="currentColor"/>'),
    more: p('<circle cx="5" cy="12" r="1.4" fill="currentColor"/><circle cx="12" cy="12" r="1.4" fill="currentColor"/><circle cx="19" cy="12" r="1.4" fill="currentColor"/>'),
    expand: p('<path d="M15 3h6v6M9 21H3v-6M21 3l-7 7M3 21l7-7"/>'),
    sliders: p('<path d="M4 21v-7M4 10V3M12 21v-9M12 8V3M20 21v-5M20 12V3M1 14h6M9 8h6M17 16h6"/>'),
    camera: p('<path d="M23 19a2 2 0 0 1-2 2H3a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h4l2-3h6l2 3h4a2 2 0 0 1 2 2z"/><circle cx="12" cy="13" r="4"/>'),
    x: p('<path d="M18 6 6 18M6 6l12 12"/>'),
    battery: p('<rect x="2" y="7" width="17" height="10" rx="2"/><path d="M22 11v2"/>'),
    bolt: p('<path d="M13 2 4 14h7l-1 8 9-12h-7z" fill="currentColor" stroke="none"/>'),
    rotate: p('<rect x="2" y="10" width="11" height="11" rx="1.5"/><path d="M8 4h4a7 7 0 0 1 7 7v2.5"/><path d="m16 11 3 3 3-3"/>'),
    refresh: p('<path d="M21 12a9 9 0 1 1-2.6-6.4"/><path d="M21 3v6h-6"/>'),
    headset: p('<rect x="2" y="7" width="20" height="11" rx="4"/><circle cx="8" cy="12.5" r="2"/><circle cx="16" cy="12.5" r="2"/>'),
    menu: p('<path d="M3 6h18M3 12h18M3 18h18"/>'),
    eye: p('<path d="M1 12s4-7 11-7 11 7 11 7-4 7-11 7S1 12 1 12z"/><circle cx="12" cy="12" r="3"/>'),
  };

  window.icon = (name) => ICONS[name] || '';
  window.hydrateIcons = (root = document) => {
    for (const el of root.querySelectorAll('i[data-icon]')) {
      if (!el.firstChild) el.innerHTML = ICONS[el.dataset.icon] || '';
    }
  };
})();
