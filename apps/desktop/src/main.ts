import './app.css';

import { mount } from 'svelte';

import App from './App.svelte';
import { applyTheme, settings } from './lib/settings.svelte';

if (import.meta.env.MODE === 'mock') {
  const { install } = await import('./lib/mock');
  install();
}

applyTheme(settings.theme);
document.documentElement.lang = settings.lang;

export default mount(App, { target: document.getElementById('app')! });
