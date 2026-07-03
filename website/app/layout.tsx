import type { Metadata, Viewport } from "next";
import { Geist, Geist_Mono, Fraunces } from "next/font/google";
import "./globals.css";

const geistSans = Geist({
  variable: "--font-geist-sans",
  subsets: ["latin"],
});

const geistMono = Geist_Mono({
  variable: "--font-geist-mono",
  subsets: ["latin"],
});

const fraunces = Fraunces({
  variable: "--font-fraunces",
  subsets: ["latin"],
  style: ["normal", "italic"],
  axes: ["opsz"],
});

export const viewport: Viewport = {
  themeColor: "#A855F7",
  width: "device-width",
  initialScale: 1,
};

export const metadata: Metadata = {
  metadataBase: new URL("https://djinnai.io"),
  title: {
    default: "Djinn | From Proposal to Pull Request",
    template: "%s | Djinn",
  },
  description: "Self-hosted, Kubernetes-native platform where your team proposes and approves work, and AI agents build it. On your cluster, with your models, behind your review.",
  keywords: ["AI", "developer tools", "AI coding agents", "Kubernetes", "self-hosted", "agent orchestration", "proposals", "autonomous development", "AI spend visibility"],
  authors: [{ name: "Djinn AI, Inc." }],
  creator: "Djinn AI, Inc.",
  openGraph: {
    type: "website",
    locale: "en_US",
    url: "https://djinnai.io",
    title: "Djinn | From Proposal to Pull Request",
    description: "Self-hosted, Kubernetes-native platform where your team proposes and approves work, and AI agents build it. On your cluster, with your models, behind your review.",
    siteName: "Djinn",
    images: [
      {
        url: "/kanban.jpg",
        width: 1200,
        height: 630,
        alt: "Djinn — from proposal to pull request",
      },
    ],
  },
  twitter: {
    card: "summary_large_image",
    title: "Djinn | From Proposal to Pull Request",
    description: "Your team proposes and approves the work. AI agents build it, on your cluster, with your models, behind your review.",
    images: ["/kanban.jpg"],
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
    <html lang="en" className={`dark ${geistSans.variable} ${geistMono.variable} ${fraunces.variable}`}>
      <head>
        <script
          type="application/ld+json"
          dangerouslySetInnerHTML={{
            __html: JSON.stringify({
              "@context": "https://schema.org",
              "@type": "SoftwareApplication",
              "name": "Djinn",
              "applicationCategory": "DeveloperApplication",
              "operatingSystem": "Kubernetes (self-hosted)",
              "offers": {
                "@type": "Offer",
                "price": "0",
                "priceCurrency": "USD",
                "availability": "https://schema.org/InStock"
              },
              "description": "Kubernetes-native platform where teams propose and approve work and AI agents build it: self-hosted, bring your own models, every change behind human review.",
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
        className="antialiased bg-[#0A0A0E] text-gray-100 selection:bg-purple-500/30 selection:text-purple-200"
      >
        {children}
      </body>
    </html>
  );
}
