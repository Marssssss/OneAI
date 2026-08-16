import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import App from './App'
import './theme/tokens.css'

const el = document.getElementById('root')
if (el === null) throw new Error('web app: missing #root')
createRoot(el).render(
  <StrictMode>
    <App />
  </StrictMode>,
)
