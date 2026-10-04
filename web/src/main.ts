import { mount } from 'svelte';
import '@fontsource/inter/latin-400.css';
import '@fontsource/inter/latin-600.css';
import './style.css';
import App from './App.svelte';

const target = document.getElementById('app');
if (!target) throw new Error('Application root is missing');
mount(App, { target });
