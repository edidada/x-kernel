import { BrowserRouter, Routes, Route } from "react-router-dom";
import BlogPage from "../pages/blog/BlogPage";
import DocsPage from "../pages/docs/DocsPage";
import HomePage from "../pages/home/HomePage";
import { AppProviders } from "./providers/AppProviders";

export default function App() {
  return (
    <AppProviders>
      <BrowserRouter>
        <Routes>
          <Route path="/" element={<HomePage />} />
          <Route path="/blog" element={<BlogPage />} />
          <Route path="/blog/:slug" element={<BlogPage />} />
          <Route path="/docs/:locale/:slug" element={<DocsPage />} />
          <Route path="/docs" element={<DocsPage />} />
          <Route path="/docs/:slug" element={<DocsPage />} />
          <Route
            path="*"
            element={
              <p>
                Page not found. <a href="/">Home</a>
              </p>
            }
          />
        </Routes>
      </BrowserRouter>
    </AppProviders>
  );
}
