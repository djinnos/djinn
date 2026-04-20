import { useState } from 'react'
import { toast } from 'sonner'

import { Button } from './button'
import { Badge } from './badge'
import { Card, CardAction, CardContent, CardDescription, CardFooter, CardHeader, CardTitle } from './card'
import {
  Combobox,
  ComboboxCollection,
  ComboboxContent,
  ComboboxEmpty,
  ComboboxGroup,
  ComboboxInput,
  ComboboxItem,
  ComboboxLabel,
  ComboboxList,
  ComboboxValue,
} from './combobox'
import {
  DropdownMenu,
  DropdownMenuCheckboxItem,
  DropdownMenuContent,
  DropdownMenuGroup,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuRadioGroup,
  DropdownMenuRadioItem,
  DropdownMenuSeparator,
  DropdownMenuSub,
  DropdownMenuSubContent,
  DropdownMenuSubTrigger,
  DropdownMenuTrigger,
} from './dropdown-menu'
import { Input } from './input'
import { InputGroup, InputGroupAddon, InputGroupInput, InputGroupText } from './input-group'
import { Label } from './label'
import {
  Select,
  SelectContent,
  SelectGroup,
  SelectItem,
  SelectLabel,
  SelectSeparator,
  SelectTrigger,
  SelectValue,
} from './select'
import { Separator } from './separator'
import { Textarea } from './textarea'
import {
  Field,
  FieldContent,
  FieldDescription,
  FieldError,
  FieldGroup,
  FieldLabel,
  FieldSet,
  FieldTitle,
} from './field'
import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
  AlertDialogTrigger,
} from './alert-dialog'
import { Spinner } from './spinner'
import { CardSkeleton, Skeleton, TextSkeleton } from './skeleton'
import { LoadingButton } from './loading-button'
import { Toaster } from './sonner'

const meta = {
  title: 'UI/Primitives Gallery',
  tags: ['autodocs'],
  parameters: { layout: 'padded', backgrounds: { default: 'dark' } },
}

export default meta

export const ButtonStories = {
  args: { variant: 'default', size: 'default', disabled: false, children: 'Button' },
  argTypes: {
    variant: { control: 'select', options: ['default', 'secondary', 'outline', 'ghost', 'destructive', 'link'] },
    size: { control: 'select', options: ['xs', 'sm', 'default', 'lg', 'icon-xs', 'icon-sm', 'icon', 'icon-lg'] },
    disabled: { control: 'boolean' },
    children: { control: 'text' },
  },
  render: ({ variant, size, disabled, children }: any) => (
    <div className="space-y-3">
      <Button variant={variant} size={size} disabled={disabled}>{children}</Button>
      <div className="flex flex-wrap gap-2">{['default', 'secondary', 'outline', 'ghost', 'destructive', 'link'].map((v) => <Button key={v} variant={v as never}>{v}</Button>)}</div>
      <div className="flex flex-wrap gap-2">{['xs', 'sm', 'default', 'lg', 'icon-xs', 'icon-sm', 'icon', 'icon-lg'].map((s) => <Button key={s} size={s as never}>{s.includes('icon') ? '•' : s}</Button>)}</div>
      <Button disabled>Disabled</Button>
    </div>
  ),
}

export const BadgeStories = {
  args: { variant: 'default', children: 'Badge' },
  argTypes: {
    variant: { control: 'select', options: ['default', 'secondary', 'destructive', 'outline', 'ghost', 'link'] },
    children: { control: 'text' },
  },
  render: ({ variant, children }: any) => <div className="flex flex-wrap gap-2"><Badge variant={variant}>{children}</Badge>{['default', 'secondary', 'destructive', 'outline', 'ghost', 'link'].map((v) => <Badge key={v} variant={v as never}>{v}</Badge>)}</div>
}

export const CardStories = { render: () => <div className="grid max-w-xl gap-4"><Card><CardHeader><CardTitle>Default card</CardTitle><CardDescription>Description text.</CardDescription><CardAction><Badge>Action</Badge></CardAction></CardHeader><CardContent>Content area</CardContent><CardFooter className="border-t"><Button size="sm">Continue</Button></CardFooter></Card><Card size="sm"><CardHeader><CardTitle>Small card</CardTitle></CardHeader><CardContent>Compact state</CardContent></Card></div> }

export const InputStories = {
  args: { placeholder: 'Type here', disabled: false, invalid: false },
  argTypes: {
    placeholder: { control: 'text' },
    disabled: { control: 'boolean' },
    invalid: { control: 'boolean' },
  },
  render: ({ placeholder, disabled, invalid }: any) => <div className="max-w-sm space-y-2"><Input placeholder={placeholder} disabled={disabled} aria-invalid={invalid || undefined} /><Input aria-invalid placeholder="Invalid state" /><Input disabled placeholder="Disabled" /></div>
}

export const InputGroupStories = { render: () => <div className="max-w-sm space-y-2"><InputGroup><InputGroupAddon><InputGroupText>https://</InputGroupText></InputGroupAddon><InputGroupInput placeholder="domain.com" /></InputGroup><InputGroup><InputGroupInput placeholder="Search" /><InputGroupAddon align="inline-end"><InputGroupText>⌘K</InputGroupText></InputGroupAddon></InputGroup></div> }

export const LabelStories = { render: () => <div className="space-y-2"><Label htmlFor="l1">Default label</Label><Input id="l1" /><div className="group" data-disabled><Label>Disabled group label</Label></div></div> }

export const TextareaStories = {
  args: { placeholder: 'Tell us more', disabled: false, invalid: false },
  argTypes: {
    placeholder: { control: 'text' },
    disabled: { control: 'boolean' },
    invalid: { control: 'boolean' },
  },
  render: ({ placeholder, disabled, invalid }: any) => <div className="max-w-sm space-y-2"><Textarea placeholder={placeholder} disabled={disabled} aria-invalid={invalid || undefined} /><Textarea aria-invalid placeholder="Invalid textarea" /><Textarea disabled placeholder="Disabled textarea" /></div>
}

export const SeparatorStories = { render: () => <div className="max-w-sm space-y-3"><div>A</div><Separator /><div>B</div><div className="flex h-12 items-center gap-2"><span>Left</span><Separator orientation="vertical" /><span>Right</span></div></div> }

export const SpinnerStories = {
  args: { size: 'default' },
  argTypes: { size: { control: 'select', options: ['xs', 'sm', 'default', 'lg', 'xl'] } },
  render: ({ size }: any) => <div className="flex items-center gap-3"><Spinner size={size} />{['xs', 'sm', 'default', 'lg', 'xl'].map((s) => <Spinner key={s} size={s as never} />)}</div>
}

export const SkeletonStories = { render: () => <div className="max-w-md space-y-3"><Skeleton className="h-6 w-40" /><TextSkeleton lines={3} /><CardSkeleton /></div> }

export const LoadingButtonStories = {
  args: { loading: true, loadingPosition: 'start', loadingText: 'Loading...', children: 'Submit' },
  argTypes: {
    loading: { control: 'boolean' },
    loadingPosition: { control: 'select', options: ['start', 'end', 'center'] },
    loadingText: { control: 'text' },
    children: { control: 'text' },
  },
  render: ({ loading, loadingPosition, loadingText, children }: any) => <div className="flex flex-wrap gap-2"><LoadingButton loading={loading} loadingPosition={loadingPosition} loadingText={loadingText}>{children}</LoadingButton><LoadingButton loading loadingPosition="end">Saving</LoadingButton><LoadingButton loading loadingPosition="center" loadingText="Please wait" className="h-12 w-32" /></div>
}

export const SelectStories = { render: () => <div className="max-w-sm"><Select defaultValue="one"><SelectTrigger className="w-full"><SelectValue placeholder="Choose" /></SelectTrigger><SelectContent><SelectGroup><SelectLabel>Options</SelectLabel><SelectItem value="one">One</SelectItem><SelectItem value="two">Two</SelectItem><SelectSeparator /><SelectItem value="three">Three</SelectItem></SelectGroup></SelectContent></Select></div> }

export const ComboboxStories = {
  args: { placeholder: 'Pick framework' },
  argTypes: { placeholder: { control: 'text' } },
  render: ({ placeholder }: any) => {
    const options = ['React', 'Vue', 'Svelte', 'Solid']
    return <div className="max-w-sm"><Combobox items={options} defaultValue={options[0]}><ComboboxInput showClear showTrigger placeholder={placeholder} /><ComboboxContent><ComboboxList><ComboboxEmpty>No results</ComboboxEmpty><ComboboxCollection>{(option: string) => <ComboboxItem key={option} value={option}>{option}</ComboboxItem>}</ComboboxCollection><ComboboxGroup><ComboboxLabel>More</ComboboxLabel><ComboboxItem value="Angular">Angular</ComboboxItem></ComboboxGroup></ComboboxList></ComboboxContent><ComboboxValue /></Combobox></div>
  }
}

export const DropdownMenuStories = {
  args: { destructive: false },
  argTypes: { destructive: { control: 'boolean' } },
  render: ({ destructive }: any) => {
    const [checked, setChecked] = useState(true)
    const [radio, setRadio] = useState('a')
    return <DropdownMenu><DropdownMenuTrigger render={<Button>Open menu</Button>} /><DropdownMenuContent><DropdownMenuLabel>Actions</DropdownMenuLabel><DropdownMenuGroup><DropdownMenuItem>Profile</DropdownMenuItem><DropdownMenuItem variant={destructive ? 'destructive' : 'default'}>Delete</DropdownMenuItem><DropdownMenuCheckboxItem checked={checked} onCheckedChange={setChecked}>Pinned</DropdownMenuCheckboxItem></DropdownMenuGroup><DropdownMenuSeparator /><DropdownMenuRadioGroup value={radio} onValueChange={setRadio}><DropdownMenuRadioItem value="a">Alpha</DropdownMenuRadioItem><DropdownMenuRadioItem value="b">Beta</DropdownMenuRadioItem></DropdownMenuRadioGroup><DropdownMenuSub><DropdownMenuSubTrigger>More</DropdownMenuSubTrigger><DropdownMenuSubContent><DropdownMenuItem>Sub action</DropdownMenuItem></DropdownMenuSubContent></DropdownMenuSub></DropdownMenuContent></DropdownMenu>
  }
}

export const FieldStories = { render: () => <FieldGroup><FieldSet><Field><FieldLabel htmlFor="field-input">Name</FieldLabel><FieldContent><Input id="field-input" placeholder="Jane" /><FieldDescription>Enter your full name.</FieldDescription></FieldContent></Field><Field data-invalid><FieldTitle>Email</FieldTitle><FieldContent><Input aria-invalid defaultValue="wrong" /><FieldError errors={[{ message: 'Invalid email' }]} /></FieldContent></Field></FieldSet></FieldGroup> }

export const AlertDialogStories = { render: () => <AlertDialog><AlertDialogTrigger render={<Button variant="outline">Open dialog</Button>} /><AlertDialogContent><AlertDialogHeader><AlertDialogTitle>Delete item?</AlertDialogTitle><AlertDialogDescription>This action cannot be undone.</AlertDialogDescription></AlertDialogHeader><AlertDialogFooter><AlertDialogCancel>Cancel</AlertDialogCancel><AlertDialogAction variant="destructive">Delete</AlertDialogAction></AlertDialogFooter></AlertDialogContent></AlertDialog> }

export const SonnerStories = {
  args: { message: 'Default toast', successMessage: 'Saved!' },
  argTypes: {
    message: { control: 'text' },
    successMessage: { control: 'text' },
  },
  render: ({ message, successMessage }: any) => (
    <div className="space-x-2">
      <Toaster />
      <Button onClick={() => toast(message)}>Toast</Button>
      <Button variant="secondary" onClick={() => toast.success(successMessage)}>Success</Button>
    </div>
  ),
}
