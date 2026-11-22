/**
 * Stories for {@link RecoveryCopySetupStep}, the desktop window's first-run
 * step that asks for a recovery copy of the library key. Only the desktop
 * window shows it, so the app pages the visual suite opens in a browser
 * never reach it.
 */
import type { Meta, StoryObj } from "@storybook/react-vite";
import { JoinShell } from "./JoinShell";
import { RecoveryCopySetupStep } from "./RecoveryCopy";

/**
 * The step inside the first-run frame, as the window shows it after
 * "Start a library on this computer", with a key that needs a copy.
 */
function SetupStep() {
  return (
    <JoinShell
      title="Save a recovery copy of your library key"
      lead="This computer keeps the key that protects your store sign-ins and secrets in its keychain, so backups of the library folder don't include it. Save a copy, wrapped with a password, somewhere off this computer."
    >
      <RecoveryCopySetupStep
        onDone={() => {}}
        load={() =>
          Promise.resolve({
            store: "keychain",
            recovery_copy: "unrecorded",
            needs_recovery_copy: true,
          })
        }
      />
    </JoinShell>
  );
}

const meta = {
  title: "Setup/RecoveryCopySetupStep",
  component: SetupStep,
  // The frame fills the screen as the first run does.
  parameters: { layout: "fullscreen" },
} satisfies Meta<typeof SetupStep>;

export default meta;

/** A story of the recovery copy step. */
type Story = StoryObj<typeof meta>;

/** The key is in the keychain and no copy is saved: the form shows. */
export const NeedsCopy: Story = {};
