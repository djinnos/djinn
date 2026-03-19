import type { Meta, StoryObj } from '@storybook/react-vite';
import { ChatEmptyState } from './ChatEmptyState';

const meta: Meta<typeof ChatEmptyState> = {
  title: 'Chat/ChatEmptyState',
  component: ChatEmptyState,
  args: {
    onPromptClick: () => {},
  },
  decorators: [
    (Story) => (
      <div className="h-[500px] w-[700px]">
        <Story />
      </div>
    ),
  ],
};

export default meta;
type Story = StoryObj<typeof ChatEmptyState>;

export const Default: Story = {};
