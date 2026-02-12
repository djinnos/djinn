import { Metadata } from "next";
import Logo from "../../components/Logo";
import Footer from "../../components/Footer";

export const metadata: Metadata = {
  title: "Privacy Policy",
  description: "Privacy Policy for the Djinn platform and related services.",
};

export default function Privacy() {
  return (
    <div className="min-h-screen font-sans bg-bg-page text-text-primary selection:bg-brand-purple/30 flex flex-col">
      
      {/* Nav */}
      <nav className="w-full z-50 glass-nav sticky top-0">
        <div className="max-w-3xl mx-auto px-6 h-16 flex items-center justify-between">
          <Logo />
        </div>
      </nav>

      <main className="flex-grow pt-12 pb-24 px-6">
        <div className="max-w-3xl mx-auto">
          <h1 className="text-4xl font-bold mb-12 text-white">Privacy Policy</h1>

          <div className="space-y-12 text-text-secondary leading-relaxed">
            
            <section>
              <p className="mb-6">
                Djinn AI, Inc. ({"\"Djinn,\""} {"\"we,\""} {"\"us\""}) values your privacy.
              </p>
              <p>
                This Privacy Policy explains how we collect, use, and protect personal information.
              </p>
            </section>

            <section>
              <h2 className="text-xl font-bold text-white mb-4">1. Information We Collect</h2>
              <p className="mb-2">We may collect:</p>
              <ul className="list-disc pl-5 space-y-2">
                <li>name and email address</li>
                <li>account credentials</li>
                <li>usage data</li>
                <li>chat inputs and outputs</li>
                <li>technical logs (IP, device, browser)</li>
              </ul>
            </section>

            <section>
              <h2 className="text-xl font-bold text-white mb-4">2. How We Use Information</h2>
              <p className="mb-2">We use information to:</p>
              <ul className="list-disc pl-5 space-y-2">
                <li>provide the Service</li>
                <li>operate AI workflows</li>
                <li>improve functionality</li>
                <li>prevent abuse</li>
                <li>comply with legal obligations</li>
              </ul>
              <p className="mt-4 font-medium text-white">We do not sell personal data.</p>
            </section>

            <section>
              <h2 className="text-xl font-bold text-white mb-4">3. AI Processing</h2>
              <ul className="list-disc pl-5 space-y-2">
                <li>User inputs may be processed by third-party AI providers to generate outputs.</li>
                <li>Data may be temporarily stored or logged for service improvement.</li>
              </ul>
            </section>

            <section>
              <h2 className="text-xl font-bold text-white mb-4">4. Data Retention</h2>
              <p>
                We retain data only as long as necessary to provide the Service or comply with legal obligations.
              </p>
            </section>

            <section>
              <h2 className="text-xl font-bold text-white mb-4">5. Security</h2>
              <p>
                We implement reasonable security measures but cannot guarantee absolute security.
              </p>
            </section>

            <section>
              <h2 className="text-xl font-bold text-white mb-4">6. Your Rights</h2>
              <p className="mb-2">Depending on jurisdiction, you may request:</p>
              <ul className="list-disc pl-5 space-y-2">
                <li>access to your data</li>
                <li>correction</li>
                <li>deletion</li>
                <li>export</li>
              </ul>
              <p className="mt-6">
                Contact: <a href="mailto:support@djinnai.io" className="text-brand-purple hover:underline">support@djinnai.io</a>
              </p>
            </section>

            <section>
              <h2 className="text-xl font-bold text-white mb-4">7. International Users</h2>
              <p>
                Data may be processed in the United States or other jurisdictions.
              </p>
            </section>

            <section>
              <h2 className="text-xl font-bold text-white mb-4">8. Children</h2>
              <p>
                The Service is not intended for users under 18.
              </p>
            </section>

            <section>
              <h2 className="text-xl font-bold text-white mb-4">9. Changes</h2>
              <p>
                We may update this Privacy Policy. Continued use constitutes acceptance.
              </p>
            </section>

          </div>
        </div>
      </main>

      <Footer />
    </div>
  );
}
