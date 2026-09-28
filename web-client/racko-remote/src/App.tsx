import { Navigate, Route, Routes } from 'react-router-dom';

import { ConnectPage } from './pages/ConnectPage';
import { DashboardPage } from './pages/DashboardPage';
import { ModeBSession } from './pages/ModeBSession';
import { SessionPage } from './pages/SessionPage';

export function App() {
  return (
    <Routes>
      <Route path="/" element={<ConnectPage />} />
      <Route path="/session" element={<SessionPage />} />
      <Route path="/modeb" element={<ModeBSession />} />
      <Route path="/dashboard" element={<DashboardPage />} />
      <Route path="*" element={<Navigate to="/" replace />} />
    </Routes>
  );
}
