import { Metadata } from "next";
import Logo from "../../components/Logo";
import Footer from "../../components/Footer";

export const metadata: Metadata = {
  title: "Terms of Service",
  description: "Terms of Service for using the Djinn platform and related services.",
};

export default function Terms() {
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
          <h1 className="text-4xl font-bold mb-12 text-white">Terms of Service</h1>

          <div className="space-y-12 text-text-secondary leading-relaxed">
            
            <section>
              <p className="mb-6">
                These Terms of Service ({"\"Terms\""}) govern access to and use of the Djinn platform (the {"\"Service\""}), operated by Djinn AI, Inc. ({"\"Djinn,\""} {"\"Company,\""} {"\"we,\""} {"\"us,\""} or {"\"our\""}).
              </p>
              <p>
                By creating an account or using the Service, you agree to these Terms.
              </p>
            </section>

            <section>
              <h2 className="text-xl font-bold text-white mb-4">1. Description of Service</h2>
              <ul className="list-disc pl-5 space-y-2">
                <li>Djinn provides AI-powered workflow automation, chatbot, orchestration, and related software tools.</li>
                <li>The Service may integrate with third-party artificial intelligence providers.</li>
                <li>The Service is provided on a beta or early-access basis and may change at any time.</li>
              </ul>
            </section>

            <section>
              <h2 className="text-xl font-bold text-white mb-4">2. Eligibility</h2>
              <ul className="list-disc pl-5 space-y-2">
                <li>You must be at least 18 years old and legally able to enter into binding agreements.</li>
                <li>If using the Service on behalf of a company, you represent you are authorized to bind that company.</li>
              </ul>
            </section>

            <section>
              <h2 className="text-xl font-bold text-white mb-4">3. Account Registration</h2>
              <p className="mb-2">You are responsible for:</p>
              <ul className="list-disc pl-5 space-y-2">
                <li>maintaining confidentiality of credentials</li>
                <li>activity under your account</li>
                <li>ensuring authorized access only</li>
              </ul>
              <p className="mt-4">We may suspend or terminate accounts at our discretion.</p>
            </section>

            <section>
              <h2 className="text-xl font-bold text-white mb-4">4. License Grant</h2>
              <p className="mb-4">
                Subject to these Terms, Djinn grants you a limited, non-exclusive, non-transferable, revocable license to access and use the Service for internal business or personal use.
              </p>
              <p className="mb-2">You may not:</p>
              <ul className="list-disc pl-5 space-y-2">
                <li>reverse engineer</li>
                <li>resell or sublicense</li>
                <li>create derivative works</li>
                <li>scrape or extract source code</li>
                <li>use the Service for unlawful purposes</li>
              </ul>
              <p className="mt-4">No ownership rights are transferred.</p>
            </section>

            <section>
              <h2 className="text-xl font-bold text-white mb-4">5. Intellectual Property</h2>
              <p className="mb-4">
                All right, title, and interest in the Service — including software, models, prompts, architecture, workflows, APIs, documentation, and improvements — remain exclusively with Djinn.
              </p>
              <p className="mb-4">
                User feedback may be used by Djinn without restriction or compensation.
              </p>
              <p>
                Use of the Service does not create joint ownership, partnership, or implied IP rights.
              </p>
            </section>

            <section>
              <h2 className="text-xl font-bold text-white mb-4">6. User Content</h2>
              <p className="mb-4">
                You retain ownership of content you submit ({"\"User Content\""}).
              </p>
              <p className="mb-4">
                You grant Djinn a limited license to process, store, and analyze User Content solely to provide and improve the Service.
              </p>
              <p className="mb-2">You represent that:</p>
              <ul className="list-disc pl-5 space-y-2">
                <li>you have rights to submit the content</li>
                <li>it does not violate third-party rights</li>
                <li>it does not contain unlawful material</li>
              </ul>
            </section>

            <section>
              <h2 className="text-xl font-bold text-white mb-4">7. AI Output Disclaimer</h2>
              <p className="mb-4">
                The Service may generate automated outputs using artificial intelligence.
              </p>
              <p className="mb-2">You acknowledge:</p>
              <ul className="list-disc pl-5 space-y-2">
                <li>outputs may be inaccurate or incomplete</li>
                <li>outputs are provided “as-is”</li>
                <li>Djinn makes no guarantees regarding accuracy</li>
                <li>you are responsible for reviewing and validating outputs</li>
              </ul>
              <p className="mt-4">
                The Service is not legal, financial, medical, or professional advice.
              </p>
            </section>

            <section>
              <h2 className="text-xl font-bold text-white mb-4">8. Third-Party Services</h2>
              <ul className="list-disc pl-5 space-y-2">
                <li>The Service may integrate with third-party providers (e.g., AI model providers, APIs).</li>
                <li>Djinn is not responsible for third-party services.</li>
                <li>Use of third-party services may be subject to their terms.</li>
              </ul>
            </section>

            <section>
              <h2 className="text-xl font-bold text-white mb-4">9. Fees</h2>
              <ul className="list-disc pl-5 space-y-2">
                <li>The Service may be offered under free trial or paid plans.</li>
                <li>Djinn may modify pricing at any time.</li>
              </ul>
            </section>

            <section>
              <h2 className="text-xl font-bold text-white mb-4">10. Confidentiality</h2>
              <p>
                You agree not to disclose non-public features or functionality without written permission.
              </p>
            </section>

            <section>
              <h2 className="text-xl font-bold text-white mb-4">11. Limitation of Liability</h2>
              <p className="mb-4">To the maximum extent permitted by law:</p>
              <ul className="list-disc pl-5 space-y-2">
                <li>Djinn shall not be liable for indirect, incidental, special, consequential, or punitive damages.</li>
                <li>Total liability shall not exceed the amount paid by you to Djinn in the preceding twelve (12) months.</li>
                <li>The Service is provided “AS IS” without warranties.</li>
              </ul>
            </section>

            <section>
              <h2 className="text-xl font-bold text-white mb-4">12. Indemnification</h2>
              <p className="mb-2">You agree to indemnify and hold Djinn harmless from claims arising from:</p>
              <ul className="list-disc pl-5 space-y-2">
                <li>your use of the Service</li>
                <li>violation of these Terms</li>
                <li>infringement caused by your User Content</li>
              </ul>
            </section>

            <section>
              <h2 className="text-xl font-bold text-white mb-4">13. Termination</h2>
              <ul className="list-disc pl-5 space-y-2">
                <li>Djinn may suspend or terminate access at any time.</li>
                <li>Upon termination, your license ends immediately.</li>
              </ul>
            </section>

            <section>
              <h2 className="text-xl font-bold text-white mb-4">14. Governing Law</h2>
              <p>
                These Terms are governed by the laws of the State of Delaware, without regard to conflict-of-law principles.
              </p>
            </section>

            <section>
              <h2 className="text-xl font-bold text-white mb-4">15. Modifications</h2>
              <p>
                Djinn may update these Terms at any time. Continued use constitutes acceptance.
              </p>
            </section>

          </div>
        </div>
      </main>

      <Footer />
    </div>
  );
}
