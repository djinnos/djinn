import type { Metadata, Viewport } from "next";
import { Geist, Geist_Mono } from "next/font/google";
import "./globals.css";

const geistSans = Geist({
  variable: "--font-geist-sans",
  subsets: ["latin"],
});

const geistMono = Geist_Mono({
  variable: "--font-geist-mono",
  subsets: ["latin"],
});

export const viewport: Viewport = {
  themeColor: "#A855F7",
  width: "device-width",
  initialScale: 1,
};

export const metadata: Metadata = {
  metadataBase: new URL("https://djinnai.io"),
  title: {
    default: "Djinn | AI Development Orchestrator",
    template: "%s | Djinn",
  },
  description: "Manage AI agents like tasks, not terminal windows. Run parallel agents on your machine with any LLM — you review every line before it merges.",
  keywords: ["AI", "developer tools", "AI coding agents", "local LLM", "AI project management", "kanban", "autonomous development"],
  authors: [{ name: "Djinn AI, Inc." }],
  creator: "Djinn AI, Inc.",
  openGraph: {
    type: "website",
    locale: "en_US",
    url: "https://djinnai.io",
    title: "Djinn | AI Development Orchestrator",
    description: "Manage AI agents like tasks, not terminal windows. Run parallel agents on your machine with any LLM — you review every line before it merges.",
    siteName: "Djinn",
    images: [
      {
        url: "/og-image.png",
        width: 1200,
        height: 630,
        alt: "Djinn — AI Development Orchestrator",
      },
    ],
  },
  twitter: {
    card: "summary_large_image",
    title: "Djinn | AI Development Orchestrator",
    description: "Manage AI agents like tasks, not terminal windows. Parallel execution, any LLM, your machine.",
    images: ["/og-image.png"],
    creator: "@djinnos", 
  },
  icons: {
    icon: "/favicon.ico",
    shortcut: "/favicon.ico",
    apple: "/logo.png",
  },
};

export default function RootLayout({
  children,
}: Readonly<{
  children: React.ReactNode;
}>) {
  return (
    <html lang="en" className="dark">
      <head>
        <script
          type="application/ld+json"
          dangerouslySetInnerHTML={{
            __html: JSON.stringify({
              "@context": "https://schema.org",
              "@type": "SoftwareApplication",
              "name": "Djinn",
              "applicationCategory": "DeveloperApplication",
              "operatingSystem": "macOS, Windows, Linux",
              "offers": {
                "@type": "Offer",
                "price": "0",
                "priceCurrency": "USD",
                "availability": "https://schema.org/InStock"
              },
              "description": "AI development orchestrator — manage parallel agents on your local machine with any LLM provider. You stay in control.",
              "author": {
                "@type": "Organization",
                "name": "Djinn AI, Inc.",
                "url": "https://djinnai.io"
              }
            }),
          }}
        />
      </head>
      <body
        className={`${geistSans.variable} ${geistMono.variable} antialiased bg-[#101010] text-gray-100 selection:bg-purple-500/30 selection:text-purple-200`}
      >
        {children}
      </body>
    </html>
  );
}
