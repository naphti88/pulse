import './styles.css'

import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'

import { App } from './App'

// on Windows the material under the page is thin: the styles give the panel
// a ground of its own there (:root.win)
if (navigator.userAgent.includes('Windows')) document.documentElement.classList.add('win')

createRoot(document.getElementById('root') as HTMLElement).render(
  <StrictMode>
    <App />
  </StrictMode>,
)
