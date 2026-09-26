import { Navigate, Route, Routes } from 'react-router-dom';

import { ConnectPage } from './pages/ConnectPage';
import { DashboardPage } from './pages/DashboardPage';
import { SessionPage } from './pages/SessionPage';

export function App() {
  return (
    <Routes>
      <Route path="/" element={<ConnectPage />} />
      <Route path="/session" element={<SessionPage />} />
      <Route path="/dashboard" element={<DashboardPage />} />
      <Route path="*" element={<Navigate to="/" replace />} />
    </Routes>
  );
}
