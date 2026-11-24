/** Stories for {@link DeliverabilityReportView}: a report with fixes, and one where everything passed. */
import type { Meta, StoryObj } from "@storybook/react-vite";
import type { DeliverabilityReport } from "@/lib/api";
import { DeliverabilityReportView } from "./DeliverabilityPanel";

/** A domain with no DMARC record and an unsigned message; mail-tester not set up. */
const needsWork: DeliverabilityReport = {
  checks: [
    {
      service: "Postmark spam check",
      outcome: "passed",
      summary:
        "SpamAssassin score 0.0 (sending-domain rules left to the DNS check); under 2 passes.",
    },
    {
      service: "DNS records",
      outcome: "warning",
      summary: "SPF: found. DMARC: missing or incomplete.",
    },
    {
      service: "mail-tester.com",
      outcome: "skipped",
      summary: "Not run: set a mail-tester account in the settings to add a test delivery.",
    },
  ],
  advice: [
    {
      outcome: "warning",
      title: "Publish a DMARC record",
      detail:
        "Add a TXT record at _dmarc.example.com, for example v=DMARC1; p=none; rua=mailto:postmaster@example.com.",
      sources: ["DNS records"],
    },
    {
      outcome: "passed",
      title: "SPF record",
      detail: "example.com publishes one SPF record ending in -all.",
      sources: ["DNS records"],
    },
  ],
};

/** Every checker passed. */
const allPassed: DeliverabilityReport = {
  checks: [
    { service: "Postmark spam check", outcome: "passed", summary: "SpamAssassin score 0.0." },
    { service: "DNS records", outcome: "passed", summary: "SPF: found. DMARC: found." },
    {
      service: "mail-tester.com",
      outcome: "passed",
      summary: "Scored 9.8 out of 10.",
      report_url: "https://www.mail-tester.com/example-1a2b3c",
    },
  ],
  advice: [],
};

const meta = {
  title: "Settings/DeliverabilityReport",
  component: DeliverabilityReportView,
  args: { report: needsWork },
} satisfies Meta<typeof DeliverabilityReportView>;

export default meta;

/** A story of {@link DeliverabilityReportView}. */
type Story = StoryObj<typeof meta>;

/** A missing DMARC record to fix, and a check that was not set up. */
export const NeedsWork: Story = {};

/** Every check passed, with mail-tester's full report linked. */
export const AllPassed: Story = { args: { report: allPassed } };
